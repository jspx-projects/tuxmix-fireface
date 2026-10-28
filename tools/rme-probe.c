// SPDX-License-Identifier: GPL-3.0-or-later
//
// rme-probe — Phase 0 characterisation tool for RME Fireface UC/UCX.
//
// Read-only by default. It can dump descriptors, issue vendor control reads,
// and capture raw audio packets from IF0 (isochronous) or IF5 (interrupt).
//
// Build: make
// Run:   ./rme-probe info
//        ./rme-probe fw
//        ./rme-probe rate
//        ./rme-probe status
//        ./rme-probe stream --iface 0 --alt 1 --seconds 2 --out cap-if0.bin
//        ./rme-probe stream --iface 5 --alt 1 --seconds 2 --out cap-if5.bin
//
// Permission: install the udev rule once with `sudo ./scripts/setup.sh`.

#define _GNU_SOURCE
#include <errno.h>
#include <inttypes.h>
#include <math.h>
#include <signal.h>
#include <stdarg.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#include "libusb.h"

#define RME_VID_OLD 0x0424u
#define RME_VID_NEW 0x2a39u
#define PID_UCX     0x3fc9u
#define PID_UC      0x3fc6u

#define ARRAY_SIZE(a) (sizeof(a) / sizeof((a)[0]))

static volatile sig_atomic_t g_stop;

static void on_signal(int sig)
{
	(void)sig;
	g_stop = 1;
}

static uint64_t now_ns(void)
{
	struct timespec ts;

	clock_gettime(CLOCK_MONOTONIC, &ts);
	return (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;
}

static void die(const char *fmt, ...)
{
	va_list ap;

	va_start(ap, fmt);
	vfprintf(stderr, fmt, ap);
	va_end(ap);
	fputc('\n', stderr);
	exit(1);
}

static void hexdump(const void *data, size_t len, size_t max)
{
	const unsigned char *p = data;
	size_t n = len < max ? len : max;

	for (size_t i = 0; i < n; i++) {
		if (i && !(i % 16))
			fputc('\n', stdout);
		printf("%02x ", p[i]);
	}
	if (len > max)
		printf("... (%zu bytes)", len);
	fputc('\n', stdout);
}

static uint32_t le32(const unsigned char *p)
{
	return (uint32_t)p[0] | ((uint32_t)p[1] << 8) |
	       ((uint32_t)p[2] << 16) | ((uint32_t)p[3] << 24);
}

static const char *speed_name(int speed)
{
	switch (speed) {
	case LIBUSB_SPEED_LOW: return "low";
	case LIBUSB_SPEED_FULL: return "full";
	case LIBUSB_SPEED_HIGH: return "high";
	case LIBUSB_SPEED_SUPER: return "super";
	case LIBUSB_SPEED_SUPER_PLUS: return "super+";
	default: return "unknown";
	}
}

static const char *xfer_name(uint8_t attr)
{
	switch (attr & LIBUSB_TRANSFER_TYPE_MASK) {
	case LIBUSB_TRANSFER_TYPE_CONTROL: return "control";
	case LIBUSB_TRANSFER_TYPE_ISOCHRONOUS: return "iso";
	case LIBUSB_TRANSFER_TYPE_BULK: return "bulk";
	case LIBUSB_TRANSFER_TYPE_INTERRUPT: return "interrupt";
	default: return "?";
	}
}

static void print_string(libusb_device_handle *h, uint8_t idx, const char *label)
{
	unsigned char buf[256];
	int r;

	if (!idx)
		return;
	r = libusb_get_string_descriptor_ascii(h, idx, buf, sizeof(buf));
	if (r > 0)
		printf("  %-12s %s\n", label, buf);
}

static libusb_device_handle *open_first(libusb_context *ctx, uint16_t vid,
					uint16_t pid)
{
	libusb_device **list = NULL;
	libusb_device *found = NULL;
	libusb_device_handle *h = NULL;
	ssize_t n;
	int r;

	n = libusb_get_device_list(ctx, &list);
	if (n < 0)
		die("libusb_get_device_list: %s", libusb_error_name((int)n));

	for (ssize_t i = 0; i < n; i++) {
		struct libusb_device_descriptor d;

		if (libusb_get_device_descriptor(list[i], &d) < 0)
			continue;
		if (d.idVendor == vid && d.idProduct == pid) {
			found = list[i];
			break;
		}
	}
	if (!found) {
		libusb_free_device_list(list, 1);
		die("device %04x:%04x not found (is it plugged in?)", vid, pid);
	}

	r = libusb_open(found, &h);
	if (r < 0) {
		libusb_free_device_list(list, 1);
		if (r == LIBUSB_ERROR_ACCESS)
			die("permission denied opening %04x:%04x\n"
			    "Install the udev rule once:  sudo ./scripts/setup.sh",
			    vid, pid);
		die("libusb_open: %s", libusb_error_name(r));
	}

	libusb_free_device_list(list, 1);
	return h;
}

static void cmd_info(libusb_context *ctx, uint16_t vid, uint16_t pid)
{
	libusb_device_handle *h = open_first(ctx, vid, pid);
	libusb_device *dev = libusb_get_device(h);
	struct libusb_device_descriptor dd;
	struct libusb_config_descriptor *cfg = NULL;
	int r;

	r = libusb_get_device_descriptor(dev, &dd);
	if (r < 0)
		die("get_device_descriptor: %s", libusb_error_name(r));

	printf("Device %04x:%04x  bus %u address %u  speed %s\n",
	       dd.idVendor, dd.idProduct, libusb_get_bus_number(dev),
	       libusb_get_device_address(dev),
	       speed_name(libusb_get_device_speed(dev)));
	printf("  bcdUSB %u.%02u  bDeviceClass 0x%02x  bcdDevice %u.%02u\n",
	       dd.bcdUSB >> 8, dd.bcdUSB & 0xff, dd.bDeviceClass,
	       dd.bcdDevice >> 8, dd.bcdDevice & 0xff);
	print_string(h, dd.iManufacturer, "Manufacturer");
	print_string(h, dd.iProduct, "Product");
	print_string(h, dd.iSerialNumber, "Serial");

	r = libusb_get_config_descriptor(dev, 0, &cfg);
	if (r < 0)
		die("get_config_descriptor: %s", libusb_error_name(r));

	printf("Configuration %u: %u interfaces, %s powered, %u mA\n",
	       cfg->bConfigurationValue, cfg->bNumInterfaces,
	       (cfg->bmAttributes & 0x40) ? "self" : "bus",
	       cfg->MaxPower * 2);

	for (unsigned int i = 0; i < cfg->bNumInterfaces; i++) {
		const struct libusb_interface *iface = &cfg->interface[i];

		for (int a = 0; a < iface->num_altsetting; a++) {
			const struct libusb_interface_descriptor *id =
				&iface->altsetting[a];

			printf("  IF %u alt %u: class 0x%02x/0x%02x/0x%02x, "
			       "%u endpoint(s)",
			       id->bInterfaceNumber, id->bAlternateSetting,
			       id->bInterfaceClass, id->bInterfaceSubClass,
			       id->bInterfaceProtocol, id->bNumEndpoints);
			if (id->iInterface)
				printf(" (string %u)", id->iInterface);
			putchar('\n');

			for (int e = 0; e < id->bNumEndpoints; e++) {
				const struct libusb_endpoint_descriptor *ed =
					&id->endpoint[e];
				unsigned int raw = ed->wMaxPacketSize;

				printf("      ep 0x%02x %-3s %-9s max %u "
				       "(raw 0x%04x, mult %u) interval %u\n",
				       ed->bEndpointAddress,
				       (ed->bEndpointAddress & 0x80) ? "IN" : "OUT",
				       xfer_name(ed->bmAttributes),
				       raw & 0x7ff, raw, (raw >> 11) & 3,
				       ed->bInterval);
			}
		}
	}

	libusb_free_config_descriptor(cfg);
	libusb_close(h);
}

static int find_in_endpoint(libusb_device_handle *h, int iface, int alt,
			    uint8_t *ep, uint16_t *maxpkt, uint8_t *attr,
			    uint8_t *interval)
{
	libusb_device *dev = libusb_get_device(h);
	struct libusb_config_descriptor *cfg = NULL;
	int r, found = -1;

	r = libusb_get_active_config_descriptor(dev, &cfg);
	if (r < 0)
		return r;

	for (unsigned int i = 0; i < cfg->bNumInterfaces && found < 0; i++) {
		const struct libusb_interface *ifc = &cfg->interface[i];

		for (int a = 0; a < ifc->num_altsetting; a++) {
			const struct libusb_interface_descriptor *id =
				&ifc->altsetting[a];

			if ((int)id->bInterfaceNumber != iface ||
			    (int)id->bAlternateSetting != alt)
				continue;

			for (int e = 0; e < id->bNumEndpoints; e++) {
				const struct libusb_endpoint_descriptor *ed =
					&id->endpoint[e];

				if (!(ed->bEndpointAddress & 0x80))
					continue;
				*ep = ed->bEndpointAddress;
				*maxpkt = ed->wMaxPacketSize & 0x7ff;
				*attr = ed->bmAttributes;
				*interval = ed->bInterval;
				found = 0;
				break;
			}
		}
	}

	libusb_free_config_descriptor(cfg);
	return found;
}

static int ctl_in(libusb_device_handle *h, uint8_t req, uint16_t value,
		  uint16_t index, unsigned char *buf, uint16_t len)
{
	return libusb_control_transfer(h, 0xc0, req, value, index, buf, len, 1000);
}

static int ctl_out(libusb_device_handle *h, uint8_t req, uint16_t value,
		   uint16_t index)
{
	return libusb_control_transfer(h, 0x40, req, value, index, NULL, 0, 1000);
}

static void cmd_fw(libusb_context *ctx, uint16_t vid, uint16_t pid)
{
	libusb_device_handle *h = open_first(ctx, vid, pid);
	unsigned char buf[8] = { 0 };
	int r;

	r = ctl_in(h, 0x1c, 0, 0, buf, 4);
	if (r < 0)
		die("control 0x1c: %s", libusb_error_name(r));
	printf("firmware raw: ");
	hexdump(buf, r, 16);
	printf("firmware version: %u\n", le32(buf) >> 16);
	libusb_close(h);
}

static void read_and_print(libusb_device_handle *h, const char *name, uint8_t req,
			   uint16_t len)
{
	unsigned char buf[256] = { 0 };
	int r;

	if (len > sizeof(buf))
		len = sizeof(buf);
	r = ctl_in(h, req, 0, 0, buf, len);
	if (r < 0) {
		printf("%s (req 0x%02x, len %u): %s\n", name, req, len,
		       libusb_error_name(r));
		return;
	}
	printf("%s (req 0x%02x, len %u): ", name, req, len);
	hexdump(buf, r, 64);
	if (r >= 4)
		printf("  as u32: %u (0x%08x)\n", le32(buf), le32(buf));
}

static void cmd_rate(libusb_context *ctx, uint16_t vid, uint16_t pid)
{
	libusb_device_handle *h = open_first(ctx, vid, pid);

	read_and_print(h, "rate 0x11", 0x11, 4);
	read_and_print(h, "rate 0x10", 0x10, 4);
	libusb_close(h);
}

static void cmd_status(libusb_context *ctx, uint16_t vid, uint16_t pid)
{
	libusb_device_handle *h = open_first(ctx, vid, pid);

	read_and_print(h, "status 0x19 len 4", 0x19, 4);
	read_and_print(h, "status 0x19 len 16", 0x19, 16);
	read_and_print(h, "status 0x19 len 64", 0x19, 64);
	libusb_close(h);
}

static void cmd_write(libusb_context *ctx, uint16_t vid, uint16_t pid,
		      int argc, char **argv)
{
	unsigned long req, value = 0, index = 0;
	bool yes = false;
	libusb_device_handle *h;
	int r;

	if (argc < 1)
		die("usage: rme-probe write <req> [value] [index] --yes");
	req = strtoul(argv[0], NULL, 0);
	if (argc > 1 && strcmp(argv[1], "--yes"))
		value = strtoul(argv[1], NULL, 0);
	if (argc > 2 && strcmp(argv[2], "--yes"))
		index = strtoul(argv[2], NULL, 0);
	for (int i = 0; i < argc; i++)
		if (!strcmp(argv[i], "--yes"))
			yes = true;
	if (!yes)
		die("refusing to write without --yes");

	h = open_first(ctx, vid, pid);
	r = ctl_out(h, (uint8_t)req, (uint16_t)value, (uint16_t)index);
	if (r < 0)
		die("write 0x%02lx: %s", req, libusb_error_name(r));
	printf("wrote 0x%02lx value=0x%04lx index=0x%04lx\n",
	       req, value, index);
	libusb_close(h);
}

static void cmd_bulk(libusb_context *ctx, uint16_t vid, uint16_t pid,
		     int argc, char **argv)
{
	int iface = 1, alt = 0, len = 512, count = 4, timeout = 500;
	unsigned long ep = 0x85;
	const char *out = NULL;
	bool hex = true;
	unsigned char *buf;
	libusb_device_handle *h;
	FILE *f = NULL;
	int r;

	for (int i = 0; i < argc; i++) {
		if (!strcmp(argv[i], "--iface") && i + 1 < argc)
			iface = atoi(argv[++i]);
		else if (!strcmp(argv[i], "--alt") && i + 1 < argc)
			alt = atoi(argv[++i]);
		else if (!strcmp(argv[i], "--ep") && i + 1 < argc)
			ep = strtoul(argv[++i], NULL, 0);
		else if (!strcmp(argv[i], "--len") && i + 1 < argc)
			len = atoi(argv[++i]);
		else if (!strcmp(argv[i], "--count") && i + 1 < argc)
			count = atoi(argv[++i]);
		else if (!strcmp(argv[i], "--timeout") && i + 1 < argc)
			timeout = atoi(argv[++i]);
		else if (!strcmp(argv[i], "--out") && i + 1 < argc)
			out = argv[++i];
		else if (!strcmp(argv[i], "--no-hex"))
			hex = false;
		else
			die("unknown bulk option: %s", argv[i]);
	}
	if (len <= 0 || len > (1 << 20))
		die("invalid --len");
	if (count <= 0 || count > 100000)
		die("invalid --count");

	h = open_first(ctx, vid, pid);
	libusb_set_auto_detach_kernel_driver(h, 1);
	r = libusb_claim_interface(h, iface);
	if (r < 0)
		die("claim IF%d: %s", iface, libusb_error_name(r));
	if (alt) {
		r = libusb_set_interface_alt_setting(h, iface, alt);
		if (r < 0)
			die("set IF%d alt %d: %s", iface, alt,
			    libusb_error_name(r));
	}

	buf = malloc((size_t)len);
	if (!buf)
		die("out of memory");
	if (out) {
		f = fopen(out, "wb");
		if (!f)
			die("cannot open %s: %s", out, strerror(errno));
	}

	printf("bulk IF%d alt %d ep 0x%02lx, len %d, count %d\n",
	       iface, alt, ep, len, count);
	for (int i = 0; i < count; i++) {
		int xfer = 0;

		r = libusb_bulk_transfer(h, (unsigned char)ep, buf,
					 (int)len, &xfer, timeout);
		if (r < 0) {
			printf("  [%d] %s\n", i, libusb_error_name(r));
			continue;
		}
		printf("  [%d] %d bytes\n", i, xfer);
		if (hex)
			hexdump(buf, (size_t)xfer, 128);
		if (f) {
			uint32_t n = (uint32_t)xfer;

			if (fwrite(&n, sizeof(n), 1, f) != 1 ||
			    fwrite(buf, 1, (size_t)xfer, f) !=
			    (size_t)xfer)
				die("write %s: %s", out, strerror(errno));
		}
	}

	if (f)
		fclose(f);
	free(buf);
	if (alt)
		libusb_set_interface_alt_setting(h, iface, 0);
	libusb_release_interface(h, iface);
	libusb_close(h);
}

static void cmd_raw(libusb_context *ctx, uint16_t vid, uint16_t pid,
		    int argc, char **argv)
{
	unsigned char buf[1024] = { 0 };
	unsigned long req, len = 4, value = 0, index = 0;
	libusb_device_handle *h;
	int r;

	if (argc < 1)
		die("usage: rme-probe raw <req> [len] [value] [index]");
	req = strtoul(argv[0], NULL, 0);
	if (argc > 1) len = strtoul(argv[1], NULL, 0);
	if (argc > 2) value = strtoul(argv[2], NULL, 0);
	if (argc > 3) index = strtoul(argv[3], NULL, 0);
	if (len > sizeof(buf))
		die("len too large (max %zu)", sizeof(buf));

	h = open_first(ctx, vid, pid);
	r = libusb_control_transfer(h, 0xc0, (uint8_t)req, (uint16_t)value,
				    (uint16_t)index, buf, (uint16_t)len, 1000);
	if (r < 0)
		die("control 0x%02lx: %s", req, libusb_error_name(r));
	printf("req 0x%02lx value 0x%04lx index 0x%04lx len %d:\n",
	       req, value, index, r);
	hexdump(buf, r, 256);
	libusb_close(h);
}

/*
 * Sample-rate sequence derived from the Fireface UC driver disassembly
 * (hwSetSampleRate.c).  For the UC/UCX the sequence is:
 *
 *   0x40/0x1b  wValue = high16(p1)  wIndex = (p1 & 0xff00) | flag
 *   0x40/0x10  wValue = p2           wIndex = 0x30
 *
 * where p1 = (0x5f5e10000000 / rate) with the 2x/4x shifts for the
 * higher rates, and (flag, p2) select the base-rate family:
 *
 *   32k/64k/128k -> flag 0, p2 0x00
 *   44.1k/88.2k/176.4k -> flag 1, p2 0x10
 *   48k/96k/192k and everything else -> flag 2, p2 0x20
 */
static void rate_params(uint32_t rate, uint16_t *value, uint16_t *index,
			uint16_t *p2_value, uint16_t *p2_index)
{
	uint64_t p1;
	uint32_t flag, p2;

	p1 = 0x5f5e10000000ull / rate;
	if (rate > 0xdea8)		/* 57000 */
		p1 <<= 1;
	if (rate >= 0x1bd51)		/* 114001 */
		p1 <<= 2;

	if (rate == 176400) {
		flag = 1;
		p2 = 0x10;
	} else if (rate > 176399) {
		flag = 2;
		p2 = 0x20;
	} else if (rate == 128000) {
		flag = 0;
		p2 = 0x00;
	} else if (rate > 127999) {
		flag = 2;
		p2 = 0x20;
	} else if (rate == 88200) {
		flag = 1;
		p2 = 0x10;
	} else if (rate > 88199) {
		flag = 2;
		p2 = 0x20;
	} else if (rate == 32000) {
		flag = 0;
		p2 = 0x00;
	} else if (rate == 44100) {
		flag = 1;
		p2 = 0x10;
	} else if (rate == 64000) {
		flag = 0;
		p2 = 0x00;
	} else {
		flag = 2;
		p2 = 0x20;
	}

	*value = (uint16_t)((p1 >> 16) & 0xffff);
	*index = (uint16_t)(((p1 & 0xff00) | flag) & 0xffff);
	*p2_value = (uint16_t)p2;
	*p2_index = 0x30;		/* UC (0x3fc6) and UCX (0x3fc9) */
}

static void cmd_set_rate(libusb_context *ctx, uint16_t vid, uint16_t pid,
			 int argc, char **argv)
{
	uint32_t rate = 0;
	bool yes = false, dry = false;
	uint16_t value, index, p2_value, p2_index;
	unsigned char rb[4];
	libusb_device_handle *h;
	int r;

	for (int i = 0; i < argc; i++) {
		if (!strcmp(argv[i], "--rate") && i + 1 < argc)
			rate = (uint32_t)strtoul(argv[++i], NULL, 0);
		else if (!strcmp(argv[i], "--yes"))
			yes = true;
		else if (!strcmp(argv[i], "--dry-run"))
			dry = true;
		else
			die("unknown set-rate option: %s", argv[i]);
	}
	if (!rate)
		die("usage: rme-probe set-rate --rate 48000 [--dry-run] --yes");

	rate_params(rate, &value, &index, &p2_value, &p2_index);

	printf("rate %u: 0x1b value=0x%04x index=0x%04x; "
	       "0x10 value=0x%04x index=0x%04x\n",
	       rate, value, index, p2_value, p2_index);
	if (dry || !yes) {
		if (!dry)
			printf("refusing to write without --yes (use --dry-run to inspect)\n");
		return;
	}

	h = open_first(ctx, vid, pid);
	r = ctl_out(h, 0x1b, value, index);
	if (r < 0)
		die("0x1b: %s", libusb_error_name(r));
	r = ctl_out(h, 0x10, p2_value, p2_index);
	if (r < 0)
		die("0x10: %s", libusb_error_name(r));

	usleep(100000);
	memset(rb, 0, sizeof(rb));
	r = ctl_in(h, 0x11, 0, 0, rb, 4);
	if (r >= 0) {
		printf("readback 0x11: ");
		hexdump(rb, r, 16);
	}
	memset(rb, 0, sizeof(rb));
	r = ctl_in(h, 0x10, 0, 0, rb, 4);
	if (r >= 0) {
		printf("readback 0x10: ");
		hexdump(rb, r, 16);
	}
	libusb_close(h);
}

/* ------------------------------------------------------------------ */
/* Raw stream capture                                                  */
/* ------------------------------------------------------------------ */

struct stream_stats {
	uint64_t packets;
	uint64_t bytes;
	uint64_t errors;
	uint64_t timeouts;
	uint64_t completions;
	uint64_t done;
	uint64_t first_ns;
	uint64_t last_ns;
};

struct cap_hdr {
	char magic[4];
	uint32_t version;
	uint32_t iface;
	uint32_t alt;
	uint32_t ep;
	uint32_t pkt_len;
	uint32_t pkts_per_urb;
};

struct rec_hdr {
	uint64_t ts_ns;
	uint32_t len;
	uint32_t index;
};

static void write_record(FILE *out, struct stream_stats *st,
			 const unsigned char *data, size_t len, unsigned int index,
			 uint64_t ts)
{
	struct rec_hdr rh = {
		.ts_ns = ts,
		.len = (uint32_t)len,
		.index = index,
	};

	if (fwrite(&rh, sizeof(rh), 1, out) != 1)
		die("write record header");
	if (len && fwrite(data, 1, len, out) != len)
		die("write record data");
	st->packets++;
	st->bytes += len;
	if (!st->first_ns)
		st->first_ns = ts;
	st->last_ns = ts;
}

struct iso_ctx {
	libusb_device_handle *h;
	struct libusb_transfer *xfer[16];
	unsigned char *buf[16];
	int n;
	int pkt_len;
	int pkts;
	int hex_left;
	uint8_t ep;
	FILE *out;
	struct stream_stats st;
	bool running;
};

static void LIBUSB_CALL iso_cb(struct libusb_transfer *t)
{
	struct iso_ctx *c = t->user_data;
	uint64_t ts = now_ns();

	if (t->status == LIBUSB_TRANSFER_COMPLETED) {
		for (int i = 0; i < t->num_iso_packets; i++) {
			const struct libusb_iso_packet_descriptor *p =
				&t->iso_packet_desc[i];
			unsigned int off = (unsigned int)i * c->pkt_len;

			if (!p->actual_length)
				continue;
			write_record(c->out, &c->st, t->buffer + off,
				     p->actual_length, (unsigned int)i, ts);
			if (c->hex_left > 0) {
				printf("pkt %d len %u: ", i, p->actual_length);
				hexdump(t->buffer + off, p->actual_length, 96);
				c->hex_left--;
			}
		}
	} else if (t->status == LIBUSB_TRANSFER_TIMED_OUT) {
		c->st.timeouts++;
	} else {
		c->st.errors++;
		if (t->status != LIBUSB_TRANSFER_CANCELLED)
			fprintf(stderr, "iso transfer status %d\n", t->status);
	}

	if (c->running && !g_stop) {
		int r = libusb_submit_transfer(t);

		if (r < 0) {
			c->st.errors++;
			c->st.done++;
		}
	} else {
		c->st.done++;
	}
}

static void capture_iso(libusb_context *ctx, libusb_device_handle *h, int iface,
			int alt, uint8_t ep, int pkt_len, int seconds,
			const char *path, int hex_left)
{
	struct timeval tv = { 0, 100000 };
	struct iso_ctx c = { 0 };
	uint64_t start;
	int r;

	c.h = h;
	c.ep = ep;
	c.pkt_len = pkt_len;
	c.pkts = 8;
	c.n = 8;
	c.hex_left = hex_left;
	c.running = true;
	c.out = fopen(path, "wb");
	if (!c.out)
		die("cannot open %s: %s", path, strerror(errno));

	struct cap_hdr ch = {
		.magic = { 'R', 'M', 'E', 'P' },
		.version = 1,
		.iface = (uint32_t)iface,
		.alt = (uint32_t)alt,
		.ep = ep,
		.pkt_len = (uint32_t)pkt_len,
		.pkts_per_urb = (uint32_t)c.pkts,
	};
	if (fwrite(&ch, sizeof(ch), 1, c.out) != 1)
		die("write capture header");

	for (int i = 0; i < c.n; i++) {
		c.xfer[i] = libusb_alloc_transfer(c.pkts);
		c.buf[i] = malloc((size_t)c.pkts * pkt_len);
		if (!c.xfer[i] || !c.buf[i])
			die("out of memory");
		libusb_fill_iso_transfer(c.xfer[i], h, ep, c.buf[i],
					 c.pkts * pkt_len, c.pkts, iso_cb, &c,
					 1000);
		for (int j = 0; j < c.pkts; j++)
			c.xfer[i]->iso_packet_desc[j].length = pkt_len;
		r = libusb_submit_transfer(c.xfer[i]);
		if (r < 0)
			die("submit iso: %s", libusb_error_name(r));
	}

	printf("capturing IF%d alt %d ep 0x%02x, %d packets x %d bytes, %d URBs\n",
	       iface, alt, ep, c.pkts, pkt_len, c.n);

	start = now_ns();
	while (!g_stop && (now_ns() - start) < (uint64_t)seconds * 1000000000ull) {
		tv.tv_sec = 0;
		tv.tv_usec = 100000;
		r = libusb_handle_events_timeout_completed(ctx, &tv, NULL);
		if (r < 0)
			break;
	}

	c.running = false;
	for (int i = 0; i < c.n; i++)
		libusb_cancel_transfer(c.xfer[i]);
	for (int i = 0; i < 100 && c.st.done < (uint64_t)c.n; i++) {
		tv.tv_sec = 0;
		tv.tv_usec = 10000;
		libusb_handle_events_timeout_completed(ctx, &tv, NULL);
	}
	for (int i = 0; i < c.n; i++) {
		libusb_free_transfer(c.xfer[i]);
		free(c.buf[i]);
	}
	fclose(c.out);

	double secs = (double)(c.st.last_ns - c.st.first_ns) / 1e9;
	printf("captured %" PRIu64 " packets, %" PRIu64 " bytes, %" PRIu64
	       " errors, %" PRIu64 " timeouts in %.3f s (%.0f pkt/s, %.0f kB/s)\n",
	       c.st.packets, c.st.bytes, c.st.errors, c.st.timeouts, secs,
	       secs > 0 ? c.st.packets / secs : 0,
	       secs > 0 ? c.st.bytes / secs / 1000.0 : 0);
	printf("wrote %s\n", path);
}

static void capture_int(libusb_context *ctx, libusb_device_handle *h, int iface,
			int alt, uint8_t ep, int maxpkt, int seconds,
			const char *path, int hex_left)
{
	unsigned char *buf = malloc((size_t)maxpkt);
	struct stream_stats st = { 0 };
	uint64_t start;
	FILE *out;
	int r;

	(void)ctx;
	if (!buf)
		die("out of memory");
	out = fopen(path, "wb");
	if (!out)
		die("cannot open %s: %s", path, strerror(errno));

	struct cap_hdr ch = {
		.magic = { 'R', 'M', 'E', 'P' },
		.version = 1,
		.iface = (uint32_t)iface,
		.alt = (uint32_t)alt,
		.ep = ep,
		.pkt_len = (uint32_t)maxpkt,
		.pkts_per_urb = 1,
	};
	if (fwrite(&ch, sizeof(ch), 1, out) != 1)
		die("write capture header");

	printf("capturing IF%d alt %d ep 0x%02x, interrupt max %d bytes\n",
	       iface, alt, ep, maxpkt);
	start = now_ns();
	while (!g_stop && (now_ns() - start) < (uint64_t)seconds * 1000000000ull) {
		int transferred = 0;

		r = libusb_interrupt_transfer(h, ep, buf, maxpkt, &transferred, 1000);
		if (r == 0 && transferred > 0) {
			uint64_t ts = now_ns();

			write_record(out, &st, buf, transferred, 0, ts);
			if (hex_left > 0) {
				printf("len %d: ", transferred);
				hexdump(buf, transferred, 96);
				hex_left--;
			}
		} else if (r == LIBUSB_ERROR_TIMEOUT) {
			st.timeouts++;
		} else if (r == LIBUSB_ERROR_NO_DEVICE) {
			break;
		} else {
			st.errors++;
			usleep(10000);
		}
	}
	fclose(out);
	free(buf);

	double secs = (double)(st.last_ns - st.first_ns) / 1e9;
	printf("captured %" PRIu64 " transfers, %" PRIu64 " bytes, %" PRIu64
	       " errors, %" PRIu64 " timeouts in %.3f s (%.0f/s, %.0f kB/s)\n",
	       st.packets, st.bytes, st.errors, st.timeouts, secs,
	       secs > 0 ? st.packets / secs : 0,
	       secs > 0 ? st.bytes / secs / 1000.0 : 0);
	printf("wrote %s\n", path);
}

static void cmd_stream(libusb_context *ctx, uint16_t vid, uint16_t pid,
		       int argc, char **argv)
{
	int iface = 0, alt = 1, seconds = 2, hex_left = 4;
	const char *out = NULL;
	uint8_t ep = 0, attr = 0, interval = 0;
	uint16_t maxpkt = 0;
	libusb_device_handle *h;
	int r;

	for (int i = 0; i < argc; i++) {
		if (!strcmp(argv[i], "--iface") && i + 1 < argc)
			iface = atoi(argv[++i]);
		else if (!strcmp(argv[i], "--alt") && i + 1 < argc)
			alt = atoi(argv[++i]);
		else if (!strcmp(argv[i], "--seconds") && i + 1 < argc)
			seconds = atoi(argv[++i]);
		else if (!strcmp(argv[i], "--out") && i + 1 < argc)
			out = argv[++i];
		else if (!strcmp(argv[i], "--hex") && i + 1 < argc)
			hex_left = atoi(argv[++i]);
		else
			die("unknown stream option: %s", argv[i]);
	}
	if (iface != 0 && iface != 5)
		die("--iface must be 0 (ISO) or 5 (interrupt)");
	if (alt < 0 || alt > 3)
		die("--alt must be 0..3");
	if (seconds <= 0)
		die("--seconds must be > 0");

	h = open_first(ctx, vid, pid);
	if (!out) {
		static char path[64];

		snprintf(path, sizeof(path), "cap-if%d-alt%d.bin", iface, alt);
		out = path;
	}

	r = find_in_endpoint(h, iface, alt, &ep, &maxpkt, &attr, &interval);
	if (r < 0)
		die("find endpoint: %s", libusb_error_name(r));
	if (!ep)
		die("no IN endpoint on IF%d alt %d", iface, alt);
	if (!maxpkt)
		die("IN endpoint 0x%02x has zero max packet size (idle alt?)", ep);

	libusb_set_auto_detach_kernel_driver(h, 1);
	r = libusb_claim_interface(h, iface);
	if (r < 0)
		die("claim IF%d: %s", iface, libusb_error_name(r));

	r = libusb_set_interface_alt_setting(h, iface, alt);
	if (r < 0) {
		libusb_set_interface_alt_setting(h, iface, 0);
		r = libusb_set_interface_alt_setting(h, iface, alt);
	}
	if (r < 0)
		die("set IF%d alt %d: %s", iface, alt, libusb_error_name(r));

	printf("IF%d alt %d endpoint 0x%02x: %s, max packet %u, interval %u\n",
	       iface, alt, ep, xfer_name(attr), maxpkt, interval);

	if ((attr & LIBUSB_TRANSFER_TYPE_MASK) == LIBUSB_TRANSFER_TYPE_ISOCHRONOUS)
		capture_iso(ctx, h, iface, alt, ep, maxpkt, seconds, out, hex_left);
	else
		capture_int(ctx, h, iface, alt, ep, maxpkt, seconds, out, hex_left);

	libusb_set_interface_alt_setting(h, iface, 0);
	libusb_release_interface(h, iface);
	libusb_close(h);
}

/* ------------------------------------------------------------------ */
/* Playback + optional IN capture (IF0)                                */
/* ------------------------------------------------------------------ */

#define PLAY_URBS 8
#define PLAY_PKTS 8
#define PLAY_IN_PKT 508

struct play_ctx {
	libusb_device_handle *h;
	struct libusb_transfer *out_xfer[PLAY_URBS];
	unsigned char *out_buf[PLAY_URBS];
	struct libusb_transfer *in_xfer[PLAY_URBS];
	unsigned char *in_buf[PLAY_URBS];
	int out_pkt_len;
	int in_pkt_len;
	int frames_per_pkt;
	int format;		/* 0 = 4-byte packet header + 72-byte frames
				 * 1 = 76-byte frames (UC-style OUT) */
	int sample_format;	/* 0 = float32, 1 = int32, 2 = int24 */
	int channel;		/* first channel of the stereo test tone */
	uint32_t rate;
	double freq;
	double amp;
	uint64_t sample_index;
	uint32_t header_counter;
	uint32_t ts_base;
	uint32_t out_pos;
	int32_t ts_offset;
	FILE *in_file;
	struct stream_stats in_st;
	bool running;
	int hex_left;
	uint64_t out_submitted;
	uint64_t out_completed;
	uint64_t out_errors;
	uint64_t out_done;
	uint64_t in_done;
	uint64_t in_errors;
};

static void put_u16(unsigned char *p, uint16_t v)
{
	p[0] = (unsigned char)(v & 0xff);
	p[1] = (unsigned char)(v >> 8);
}

static void put_f32(unsigned char *p, float v)
{
	uint32_t u;

	memcpy(&u, &v, sizeof(u));
	p[0] = (unsigned char)(u & 0xff);
	p[1] = (unsigned char)((u >> 8) & 0xff);
	p[2] = (unsigned char)((u >> 16) & 0xff);
	p[3] = (unsigned char)((u >> 24) & 0xff);
}

static void put_u32(unsigned char *p, uint32_t v)
{
	p[0] = (unsigned char)(v & 0xff);
	p[1] = (unsigned char)((v >> 8) & 0xff);
	p[2] = (unsigned char)((v >> 16) & 0xff);
	p[3] = (unsigned char)((v >> 24) & 0xff);
}

static float tone_sample(const struct play_ctx *c, uint64_t idx)
{
	if (c->amp == 0.0)
		return 0.0f;
	return (float)(c->amp *
		       sin(2.0 * M_PI * c->freq * (double)idx / (double)c->rate));
}

static void put_sample(unsigned char *p, float v, int fmt)
{
	if (fmt == 1) {
		int32_t s = (int32_t)((double)v * 2147483647.0);

		put_u32(p, (uint32_t)s);
	} else if (fmt == 2) {
		int32_t s = (int32_t)((double)v * 8388607.0) << 8;

		put_u32(p, (uint32_t)s);
	} else {
		put_f32(p, v);
	}
}

static uint32_t out_ts(const struct play_ctx *c, uint64_t idx)
{
	return c->ts_base + (uint32_t)(idx << 5);
}

static char format_char(int f)
{
	static const char names[] = "abcdefghijk";

	if (f >= 0 && f <= 10)
		return names[f];
	if (f == 11)
		return 'm';
	if (f == 12)
		return 'n';
	if (f == 13)
		return 'p';
	return '?';
}

static void fill_out_urb(struct play_ctx *c, struct libusb_transfer *t)
{
	uint64_t start = c->sample_index;
	unsigned char *buf = t->buffer;
	int off = 0;

	for (int p = 0; p < t->num_iso_packets; p++) {
		unsigned char *pkt = buf + off;
		int frames = c->frames_per_pkt;

		if (c->format == 0 || c->format == 6) {
			/* 4-byte packet timestamp, then 18 float32 per frame.
			 * Format G uses the 0x4d80 high word seen in the IN
			 * stream instead of zero. */
			uint32_t ts = out_ts(c, start);

			put_u16(pkt, (uint16_t)ts);
			put_u16(pkt + 2, (uint16_t)(ts >> 16));
			for (int f = 0; f < frames; f++) {
				unsigned char *fp = pkt + 4 + f * 72;

				for (int ch = 0; ch < 18; ch++) {
					float v = (ch == c->channel ||
						   ch == c->channel + 1) ?
						tone_sample(c, start + f) : 0.0f;

					put_sample(fp + ch * 4, v, c->sample_format);
				}
			}
			t->iso_packet_desc[p].length = (uint32_t)(4 + frames * 72);
		} else if (c->format == 1) {
			/* UC-style 76-byte frames: 4-byte counter + 18 float32.
			 * The UC driver writes the same counter to every frame
			 * in a packet; it starts at 0x16 and advances by the
			 * frame count. */
			uint16_t hdr = (uint16_t)(c->header_counter & 0xffff);

			for (int f = 0; f < frames; f++) {
				unsigned char *fp = pkt + f * 76;

				put_u16(fp, hdr);
				put_u16(fp + 2, 0);
				for (int ch = 0; ch < 18; ch++) {
					float v = (ch == c->channel ||
						   ch == c->channel + 1) ?
						tone_sample(c, start + f) : 0.0f;

					put_sample(fp + 4 + ch * 4, v,
						   c->sample_format);
				}
			}
			c->header_counter += (uint32_t)frames;
			t->iso_packet_desc[p].length = (uint32_t)(frames * 76);
		} else if (c->format == 2) {
			/* 72-byte frames with no per-packet header. */
			for (int f = 0; f < frames; f++) {
				unsigned char *fp = pkt + f * 72;

				for (int ch = 0; ch < 18; ch++) {
					float v = (ch == c->channel ||
						   ch == c->channel + 1) ?
						tone_sample(c, start + f) : 0.0f;

					put_sample(fp + ch * 4, v, c->sample_format);
				}
			}
			t->iso_packet_desc[p].length = (uint32_t)(frames * 72);
		} else if (c->format == 3) {
			/* 72-byte frames followed by a 4-byte trailer. */
			for (int f = 0; f < frames; f++) {
				unsigned char *fp = pkt + f * 72;

				for (int ch = 0; ch < 18; ch++) {
					float v = (ch == c->channel ||
						   ch == c->channel + 1) ?
						tone_sample(c, start + f) : 0.0f;

					put_sample(fp + ch * 4, v, c->sample_format);
				}
			}
			uint32_t ts = out_ts(c, start);

			put_u16(pkt + frames * 72, (uint16_t)ts);
			put_u16(pkt + frames * 72 + 2, (uint16_t)(ts >> 16));
			t->iso_packet_desc[p].length =
				(uint32_t)(frames * 72 + 4);
		} else if (c->format == 7) {
			/* 76-byte frames: 72-byte audio + 4-byte trailer. */
			for (int f = 0; f < frames; f++) {
				unsigned char *fp = pkt + f * 76;

				for (int ch = 0; ch < 18; ch++) {
					float v = (ch == c->channel ||
						   ch == c->channel + 1) ?
						tone_sample(c, start + f) : 0.0f;

					put_sample(fp + ch * 4, v,
						   c->sample_format);
				}
				uint32_t ts = out_ts(c, start + f);

				put_u16(fp + 72, (uint16_t)ts);
				put_u16(fp + 74, (uint16_t)(ts >> 16));
			}
			t->iso_packet_desc[p].length = (uint32_t)(frames * 76);
		} else if (c->format == 8) {
			/* 4-byte zero header + 72-byte frames. */
			put_u16(pkt, 0);
			put_u16(pkt + 2, 0);
			for (int f = 0; f < frames; f++) {
				unsigned char *fp = pkt + 4 + f * 72;

				for (int ch = 0; ch < 18; ch++) {
					float v = (ch == c->channel ||
						   ch == c->channel + 1) ?
						tone_sample(c, start + f) : 0.0f;

					put_sample(fp + ch * 4, v,
						   c->sample_format);
				}
			}
			t->iso_packet_desc[p].length = (uint32_t)(4 + frames * 72);
		} else if (c->format == 9) {
			/* 4 + 72N with the UC-style small packet counter. */
			put_u16(pkt, (uint16_t)(c->header_counter & 0xffff));
			put_u16(pkt + 2, 0);
			for (int f = 0; f < frames; f++) {
				unsigned char *fp = pkt + 4 + f * 72;

				for (int ch = 0; ch < 18; ch++) {
					float v = (ch == c->channel ||
						   ch == c->channel + 1) ?
						tone_sample(c, start + f) : 0.0f;

					put_sample(fp + ch * 4, v,
						   c->sample_format);
				}
			}
			c->header_counter += (uint32_t)frames;
			t->iso_packet_desc[p].length = (uint32_t)(4 + frames * 72);
		} else if (c->format == 10) {
			/* 4 + 72N with the unshifted sample index. */
			uint32_t ts = (uint32_t)start;

			put_u16(pkt, (uint16_t)ts);
			put_u16(pkt + 2, (uint16_t)(ts >> 16));
			for (int f = 0; f < frames; f++) {
				unsigned char *fp = pkt + 4 + f * 72;

				for (int ch = 0; ch < 18; ch++) {
					float v = (ch == c->channel ||
						   ch == c->channel + 1) ?
						tone_sample(c, start + f) : 0.0f;

					put_sample(fp + ch * 4, v,
						   c->sample_format);
				}
			}
			t->iso_packet_desc[p].length = (uint32_t)(4 + frames * 72);
		} else if (c->format == 11) {
			/* Mac-mode OUT: N frames of (header + 18 floats).
			 * Only the first frame's header carries the 13-bit play
			 * position; the remaining headers are zero. */
			for (int f = 0; f < frames; f++) {
				unsigned char *fp = pkt + f * 76;
				uint16_t hdr = (f == 0) ?
					(uint16_t)(c->out_pos & 0x1fff) : 0;

				put_u16(fp, hdr);
				put_u16(fp + 2, 0);
				for (int ch = 0; ch < 18; ch++) {
					float v = (ch == c->channel ||
						   ch == c->channel + 1) ?
						tone_sample(c, start + f) : 0.0f;

					put_sample(fp + 4 + ch * 4, v,
						   c->sample_format);
				}
			}
			c->out_pos = (c->out_pos + (uint32_t)frames) & 0x1fff;
			t->iso_packet_desc[p].length = (uint32_t)(frames * 76);
		} else if (c->format == 12) {
			/* 4-byte packet header (13-bit position) + planar
			 * 18 x N float32, matching the IN packet layout. */
			put_u16(pkt, (uint16_t)(c->out_pos & 0x1fff));
			put_u16(pkt + 2, 0);
			for (int ch = 0; ch < 18; ch++) {
				for (int f = 0; f < frames; f++) {
					float v = (ch == c->channel ||
						   ch == c->channel + 1) ?
						tone_sample(c, start + f) : 0.0f;

					put_sample(pkt + 4 + ch * frames * 4 +
						   f * 4, v,
						   c->sample_format);
				}
			}
			c->out_pos = (c->out_pos + (uint32_t)frames) & 0x1fff;
			t->iso_packet_desc[p].length = (uint32_t)(4 + frames * 72);
		} else if (c->format == 13) {
			/* 4-byte packet header (13-bit position) + interleaved
			 * 72N float32. */
			put_u16(pkt, (uint16_t)(c->out_pos & 0x1fff));
			put_u16(pkt + 2, 0);
			for (int f = 0; f < frames; f++) {
				unsigned char *fp = pkt + 4 + f * 72;

				for (int ch = 0; ch < 18; ch++) {
					float v = (ch == c->channel ||
						   ch == c->channel + 1) ?
						tone_sample(c, start + f) : 0.0f;

					put_sample(fp + ch * 4, v,
						   c->sample_format);
				}
			}
			c->out_pos = (c->out_pos + (uint32_t)frames) & 0x1fff;
			t->iso_packet_desc[p].length = (uint32_t)(4 + frames * 72);
		} else {
			/* 76-byte frames with a 1/32-sample counter per frame.
			 * Format F additionally carries the 0x4d80 high word
			 * observed in the UCX IN stream. */
			for (int f = 0; f < frames; f++) {
				unsigned char *fp = pkt + f * 76;
				uint32_t ts = out_ts(c, start + f);

				put_u16(fp, (uint16_t)ts);
				put_u16(fp + 2, (uint16_t)(ts >> 16));
				for (int ch = 0; ch < 18; ch++) {
					float v = (ch == c->channel ||
						   ch == c->channel + 1) ?
						tone_sample(c, start + f) : 0.0f;

					put_sample(fp + 4 + ch * 4, v,
						   c->sample_format);
				}
			}
			t->iso_packet_desc[p].length = (uint32_t)(frames * 76);
		}
		start += (uint64_t)frames;
		off += (int)t->iso_packet_desc[p].length;
	}
	c->sample_index = start;
}

static void LIBUSB_CALL play_out_cb(struct libusb_transfer *t)
{
	struct play_ctx *c = t->user_data;

	if (t->status == LIBUSB_TRANSFER_COMPLETED) {
		c->out_completed++;
	} else if (t->status == LIBUSB_TRANSFER_CANCELLED) {
		c->out_done++;
		return;
	} else {
		c->out_errors++;
		if (c->out_errors <= 5)
			fprintf(stderr, "OUT iso status %d\n", t->status);
	}

	if (c->running && !g_stop) {
		fill_out_urb(c, t);
		int r = libusb_submit_transfer(t);

		if (r < 0) {
			c->out_errors++;
			c->out_done++;
			return;
		}
		c->out_submitted++;
	} else {
		c->out_done++;
	}
}

static void LIBUSB_CALL play_in_cb(struct libusb_transfer *t)
{
	struct play_ctx *c = t->user_data;
	uint64_t ts = now_ns();

	if (t->status == LIBUSB_TRANSFER_COMPLETED) {
		for (int i = 0; i < t->num_iso_packets; i++) {
			const struct libusb_iso_packet_descriptor *p =
				&t->iso_packet_desc[i];
			unsigned int off = (unsigned int)i * c->in_pkt_len;

			if (!p->actual_length)
				continue;
			if (c->in_file)
				write_record(c->in_file, &c->in_st,
					     t->buffer + off, p->actual_length,
					     (unsigned int)i, ts);
			if (c->hex_left > 0) {
				printf("IN pkt %d len %u: ", i, p->actual_length);
				hexdump(t->buffer + off, p->actual_length, 64);
				c->hex_left--;
			}
		}
	} else if (t->status == LIBUSB_TRANSFER_CANCELLED) {
		c->in_done++;
		return;
	} else {
		c->in_errors++;
	}

	if (c->running && !g_stop) {
		int r = libusb_submit_transfer(t);

		if (r < 0) {
			c->in_errors++;
			c->in_done++;
		}
	} else {
		c->in_done++;
	}
}

struct ts_read_ctx {
	bool done;
	uint32_t ts;
};

static void LIBUSB_CALL ts_read_cb(struct libusb_transfer *t)
{
	struct ts_read_ctx *r = t->user_data;

	if (t->status == LIBUSB_TRANSFER_COMPLETED &&
	    t->iso_packet_desc[0].actual_length >= 4)
		r->ts = le32(t->buffer);
	r->done = true;
}

static bool read_in_ts(libusb_context *ctx, libusb_device_handle *h,
		       uint32_t *ts)
{
	unsigned char buf[508];
	struct ts_read_ctx r = { false, 0 };
	struct libusb_transfer *t;
	bool ok = false;

	t = libusb_alloc_transfer(1);
	if (!t)
		return false;
	libusb_fill_iso_transfer(t, h, 0x8f, buf, 436, 1, ts_read_cb, &r, 1000);
	t->iso_packet_desc[0].length = 436;
	if (libusb_submit_transfer(t) < 0)
		goto out;
	for (int i = 0; i < 100 && !r.done; i++) {
		struct timeval tv = { 0, 10000 };

		libusb_handle_events_timeout_completed(ctx, &tv, NULL);
	}
	if (r.done) {
		*ts = r.ts;
		ok = true;
	}
out:
	if (!r.done)
		libusb_cancel_transfer(t);
	libusb_free_transfer(t);
	return ok;
}

static bool read_in_pos(libusb_context *ctx, libusb_device_handle *h,
			uint32_t *pos, uint32_t *frames)
{
	unsigned char buf[8 * 508];
	struct ts_read_ctx r = { false, 0 };
	struct libusb_transfer *t;
	bool ok = false;

	t = libusb_alloc_transfer(8);
	if (!t)
		return false;
	libusb_fill_iso_transfer(t, h, 0x8f, buf, sizeof(buf), 8, ts_read_cb,
				 &r, 1000);
	for (int i = 0; i < 8; i++)
		t->iso_packet_desc[i].length = 508;
	if (libusb_submit_transfer(t) < 0)
		goto out;
	for (int i = 0; i < 100 && !r.done; i++) {
		struct timeval tv = { 0, 10000 };

		libusb_handle_events_timeout_completed(ctx, &tv, NULL);
	}
	if (r.done) {
		for (int i = 0; i < 8; i++) {
			const struct libusb_iso_packet_descriptor *d =
				&t->iso_packet_desc[i];
			const unsigned char *p;
			uint32_t hdr, n;

			if (d->status || d->actual_length < 4)
				continue;
			p = buf + (unsigned)i * 508;
			hdr = le32(p);
			n = hdr & 0x1f;
			if (n > 0 && n <= 7) {
				*pos = (hdr >> 5) & 0x1fff;
				*frames = n;
				ok = true;
				break;
			}
		}
	}
out:
	if (!r.done)
		libusb_cancel_transfer(t);
	libusb_free_transfer(t);
	return ok;
}

static void cmd_play(libusb_context *ctx, uint16_t vid, uint16_t pid,
		     int argc, char **argv)
{
	struct play_ctx c = { 0 };
	struct timeval tv;
	libusb_device_handle *h;
	const char *cap = "cap-play-in.bin";
	bool unmute = true;
	bool use_in_ts = false;
	int offset = 396;
	int seconds = 3, hex_left = 2;
	uint32_t rate = 48000;
	double freq = 1000.0, amp = 0.1;
	int channel = 0, format = 13, sample_format = 0;
	int r;

	for (int i = 0; i < argc; i++) {
		if (!strcmp(argv[i], "--seconds") && i + 1 < argc)
			seconds = atoi(argv[++i]);
		else if (!strcmp(argv[i], "--rate") && i + 1 < argc)
			rate = (uint32_t)strtoul(argv[++i], NULL, 0);
		else if (!strcmp(argv[i], "--tone") && i + 1 < argc)
			freq = atof(argv[++i]);
		else if (!strcmp(argv[i], "--amp") && i + 1 < argc)
			amp = atof(argv[++i]);
		else if (!strcmp(argv[i], "--channel") && i + 1 < argc)
			channel = atoi(argv[++i]);
		else if (!strcmp(argv[i], "--format") && i + 1 < argc) {
			const char *f = argv[++i];

			if (!strcmp(f, "a"))
				format = 0;
			else if (!strcmp(f, "b"))
				format = 1;
			else if (!strcmp(f, "c"))
				format = 2;
			else if (!strcmp(f, "d"))
				format = 3;
			else if (!strcmp(f, "e"))
				format = 4;
			else if (!strcmp(f, "f"))
				format = 5;
			else if (!strcmp(f, "g"))
				format = 6;
			else if (!strcmp(f, "h"))
				format = 7;
			else if (!strcmp(f, "i"))
				format = 8;
			else if (!strcmp(f, "j"))
				format = 9;
			else if (!strcmp(f, "k"))
				format = 10;
			else if (!strcmp(f, "m"))
				format = 11;
			else if (!strcmp(f, "n"))
				format = 12;
			else if (!strcmp(f, "p"))
				format = 13;
			else
				die("--format must be a..i, j, k, m, n or p");
		} else if (!strcmp(argv[i], "--sample-format") && i + 1 < argc) {
			const char *s = argv[++i];

			if (!strcmp(s, "float"))
				sample_format = 0;
			else if (!strcmp(s, "int32"))
				sample_format = 1;
			else if (!strcmp(s, "int24"))
				sample_format = 2;
			else
				die("--sample-format must be float, int32 or int24");
		} else if (!strcmp(argv[i], "--capture") && i + 1 < argc)
			cap = argv[++i];
		else if (!strcmp(argv[i], "--no-capture"))
			cap = NULL;
		else if (!strcmp(argv[i], "--no-unmute"))
			unmute = false;
		else if (!strcmp(argv[i], "--use-in-ts"))
			use_in_ts = true;
		else if (!strcmp(argv[i], "--offset") && i + 1 < argc)
			offset = atoi(argv[++i]);
		else if (!strcmp(argv[i], "--hex") && i + 1 < argc)
			hex_left = atoi(argv[++i]);
		else
			die("unknown play option: %s", argv[i]);
	}

	if (rate != 48000)
		die("play currently implements 48 kHz only (alt 1, 6 frames/packet)");
	if (seconds <= 0)
		die("--seconds must be > 0");
	if (channel < 0 || channel > 17)
		die("--channel must be 0..17");
	if (amp < 0.0 || amp > 1.0)
		die("--amp must be 0..1");

	h = open_first(ctx, vid, pid);
	libusb_set_auto_detach_kernel_driver(h, 1);
	r = libusb_claim_interface(h, 0);
	if (r < 0)
		die("claim IF0: %s", libusb_error_name(r));
	r = libusb_set_interface_alt_setting(h, 0, 1);
	if (r < 0)
		die("set IF0 alt 1: %s", libusb_error_name(r));

	c.h = h;
	c.rate = rate;
	c.freq = freq;
	c.amp = amp;
	c.channel = channel;
	c.format = format;
	c.sample_format = sample_format;
	c.ts_base = (format == 5 || format == 6 || format == 7) ?
		0x4d800000u : 0;
	c.frames_per_pkt = 6;
	c.out_pkt_len = (format == 0 || format == 6 || format == 8 ||
			 format == 12 || format == 13) ? 4 + 6 * 72 :
			(format == 1 || format == 4 || format == 5 ||
			 format == 7 || format == 11) ? 6 * 76 :
			(format == 3) ? 6 * 72 + 4 : 6 * 72;
	c.header_counter = 0x16;
	c.in_pkt_len = PLAY_IN_PKT;
	c.hex_left = hex_left;
	c.running = true;

	if (offset >= 0) {
		uint32_t pos, n;

		if (read_in_pos(ctx, h, &pos, &n)) {
			c.out_pos = (pos + (uint32_t)offset) & 0x1fff;
			printf("OUT position: 0x%04x (IN 0x%04x + %d, %u frames)\n",
			       c.out_pos, pos, offset, n);
		} else {
			fprintf(stderr, "could not read IN position\n");
		}
	}

	if (use_in_ts) {
		uint32_t ts;

		if (read_in_ts(ctx, h, &ts)) {
			c.ts_base = ts;
			printf("OUT timestamp base: 0x%08x (from IN)\n", ts);
		} else {
			fprintf(stderr, "could not read IN timestamp\n");
		}
	}

	printf("playing %.0f Hz at %.3f amp on channels %d/%d, format %c, "
	       "%d frames/packet\n", freq, amp, channel + 1, channel + 2,
	       format_char(format), c.frames_per_pkt);

	if (unmute) {
		r = ctl_out(h, 0x14, 0xffff, 0xc000);
		if (r < 0)
			fprintf(stderr, "unmute 0x14: %s\n",
				libusb_error_name(r));
		else
			printf("unmuted (0x14 0xffff/0xc000)\n");
	}

	if (cap) {
		c.in_file = fopen(cap, "wb");
		if (!c.in_file)
			die("cannot open %s: %s", cap, strerror(errno));

		struct cap_hdr ch = {
			.magic = { 'R', 'M', 'E', 'P' },
			.version = 1,
			.iface = 0,
			.alt = 1,
			.ep = 0x8f,
			.pkt_len = PLAY_IN_PKT,
			.pkts_per_urb = PLAY_PKTS,
		};

		if (fwrite(&ch, sizeof(ch), 1, c.in_file) != 1)
			die("write capture header");
	}

	for (int i = 0; i < PLAY_URBS; i++) {
		c.out_xfer[i] = libusb_alloc_transfer(PLAY_PKTS);
		c.out_buf[i] = malloc((size_t)PLAY_PKTS * c.out_pkt_len);
		if (!c.out_xfer[i] || !c.out_buf[i])
			die("out of memory");
		libusb_fill_iso_transfer(c.out_xfer[i], h, 0x0e, c.out_buf[i],
					 PLAY_PKTS * c.out_pkt_len, PLAY_PKTS,
					 play_out_cb, &c, 1000);
		for (int j = 0; j < PLAY_PKTS; j++)
			c.out_xfer[i]->iso_packet_desc[j].length =
				(uint32_t)c.out_pkt_len;
		fill_out_urb(&c, c.out_xfer[i]);
	}

	if (c.in_file) {
		for (int i = 0; i < PLAY_URBS; i++) {
			c.in_xfer[i] = libusb_alloc_transfer(PLAY_PKTS);
			c.in_buf[i] = malloc((size_t)PLAY_PKTS * c.in_pkt_len);
			if (!c.in_xfer[i] || !c.in_buf[i])
				die("out of memory");
			libusb_fill_iso_transfer(c.in_xfer[i], h, 0x8f,
						 c.in_buf[i],
						 PLAY_PKTS * c.in_pkt_len,
						 PLAY_PKTS, play_in_cb, &c,
						 1000);
			for (int j = 0; j < PLAY_PKTS; j++)
				c.in_xfer[i]->iso_packet_desc[j].length =
					(uint32_t)c.in_pkt_len;
		}
	}

	for (int i = 0; i < PLAY_URBS; i++) {
		r = libusb_submit_transfer(c.out_xfer[i]);
		if (r < 0)
			die("submit OUT: %s", libusb_error_name(r));
		c.out_submitted++;
	}
	if (c.in_file) {
		for (int i = 0; i < PLAY_URBS; i++) {
			r = libusb_submit_transfer(c.in_xfer[i]);
			if (r < 0)
				die("submit IN: %s", libusb_error_name(r));
		}
	}

	uint64_t start = now_ns();

	while (!g_stop && (now_ns() - start) <
	       (uint64_t)seconds * 1000000000ull) {
		tv.tv_sec = 0;
		tv.tv_usec = 100000;
		r = libusb_handle_events_timeout_completed(ctx, &tv, NULL);
		if (r < 0)
			break;
	}

	c.running = false;
	for (int i = 0; i < PLAY_URBS; i++) {
		libusb_cancel_transfer(c.out_xfer[i]);
		if (c.in_file)
			libusb_cancel_transfer(c.in_xfer[i]);
	}
	for (int i = 0; i < 500; i++) {
		if (c.out_done >= PLAY_URBS &&
		    (!c.in_file || c.in_done >= PLAY_URBS))
			break;
		tv.tv_sec = 0;
		tv.tv_usec = 10000;
		libusb_handle_events_timeout_completed(ctx, &tv, NULL);
	}

	for (int i = 0; i < PLAY_URBS; i++) {
		libusb_free_transfer(c.out_xfer[i]);
		free(c.out_buf[i]);
		if (c.in_file) {
			libusb_free_transfer(c.in_xfer[i]);
			free(c.in_buf[i]);
		}
	}

	if (c.in_file)
		fclose(c.in_file);

	libusb_set_interface_alt_setting(h, 0, 0);
	libusb_release_interface(h, 0);
	libusb_close(h);

	printf("OUT: %" PRIu64 " submitted, %" PRIu64 " completed, "
	       "%" PRIu64 " errors\n", c.out_submitted, c.out_completed,
	       c.out_errors);
	if (c.in_file)
		printf("IN: %" PRIu64 " packets, %" PRIu64 " bytes, "
		       "%" PRIu64 " errors\n", c.in_st.packets, c.in_st.bytes,
		       c.in_errors);
	if (cap)
		printf("wrote %s\n", cap);
}

/*
 * Minimal RME-protocol playback test: IF5 interrupt OUT, 18 int32 channels,
 * interleaved, no header.  This is the Windows PC-mode format captured in
 * rme_totalmix7/8.pcapng.  The kernel driver keeps IF0; this claims IF5 and
 * feeds the RME mixer so its software-playback VU meters and routing can be
 * checked independently of the class-compliant IF0 path.
 */
static void cmd_pcplay(libusb_context *ctx, uint16_t vid, uint16_t pid,
		       int argc, char **argv)
{
	libusb_device_handle *h;
	int seconds = 3;
	int frames_per_xfer = 1024;
	int iface = 5, alt = 2;
	double amp = 0.25, freq_l = 440.0, freq_r = 880.0;
	uint32_t rate = 48000;
	bool unmute = true, drain_in = true;
	unsigned char *out_buf, *in_buf;
	size_t out_len, in_len;
	uint64_t frames_sent = 0;
	uint64_t start;
	int r;

	for (int i = 0; i < argc; i++) {
		if (!strcmp(argv[i], "--seconds") && i + 1 < argc)
			seconds = atoi(argv[++i]);
		else if (!strcmp(argv[i], "--frames") && i + 1 < argc)
			frames_per_xfer = atoi(argv[++i]);
		else if (!strcmp(argv[i], "--iface") && i + 1 < argc)
			iface = atoi(argv[++i]);
		else if (!strcmp(argv[i], "--alt") && i + 1 < argc)
			alt = atoi(argv[++i]);
		else if (!strcmp(argv[i], "--amp") && i + 1 < argc)
			amp = atof(argv[++i]);
		else if (!strcmp(argv[i], "--tone-l") && i + 1 < argc)
			freq_l = atof(argv[++i]);
		else if (!strcmp(argv[i], "--tone-r") && i + 1 < argc)
			freq_r = atof(argv[++i]);
		else if (!strcmp(argv[i], "--no-in"))
			drain_in = false;
		else if (!strcmp(argv[i], "--no-unmute"))
			unmute = false;
		else
			die("unknown pcplay option: %s", argv[i]);
	}

	if (seconds <= 0 || frames_per_xfer <= 0 || frames_per_xfer > 4096)
		die("--seconds must be > 0, --frames 1..4096");
	if (amp < 0.0 || amp > 1.0)
		die("--amp must be 0..1");

	out_len = (size_t)frames_per_xfer * 18 * 4;
	in_len = 73728;		/* one Windows-style IF5 IN URB */
	out_buf = malloc(out_len);
	in_buf = malloc(in_len);
	if (!out_buf || !in_buf)
		die("out of memory");

	h = open_first(ctx, vid, pid);
	libusb_set_auto_detach_kernel_driver(h, 1);
	r = libusb_claim_interface(h, iface);
	if (r < 0)
		die("claim IF%d: %s", iface, libusb_error_name(r));
	r = libusb_set_interface_alt_setting(h, iface, alt);
	if (r < 0) {
		libusb_set_interface_alt_setting(h, iface, 0);
		r = libusb_set_interface_alt_setting(h, iface, alt);
	}
	if (r < 0)
		die("set IF%d alt %d: %s", iface, alt, libusb_error_name(r));

	if (unmute) {
		r = ctl_out(h, 0x14, 0xffff, 0xc000);
		if (r < 0)
			fprintf(stderr, "unmute 0x14: %s\n",
				libusb_error_name(r));
		else
			printf("unmuted (0x14 0xffff/0xc000)\n");
	}

	printf("IF%d alt %d: %d frames/xfer, %.0f/%.0f Hz at %.3f amp, %d s\n",
	       iface, alt, frames_per_xfer, freq_l, freq_r, amp, seconds);

	start = now_ns();
	while (!g_stop &&
	       (now_ns() - start) < (uint64_t)seconds * 1000000000ull) {
		int32_t *s = (int32_t *)out_buf;
		int transferred = 0;

		for (int f = 0; f < frames_per_xfer; f++) {
			uint64_t n = frames_sent + (uint64_t)f;
			double t = (double)n / rate;

			for (int c = 0; c < 18; c++) {
				double v = 0.0;

				if (c == 0)
					v = amp * sin(2.0 * M_PI * freq_l * t);
				else if (c == 1)
					v = amp * sin(2.0 * M_PI * freq_r * t);
				s[f * 18 + c] =
					(int32_t)llrint(v * 2147483647.0);
			}
		}

		r = libusb_interrupt_transfer(h, 0x01, out_buf, (int)out_len,
					      &transferred, 1000);
		if (r < 0 && r != LIBUSB_ERROR_TIMEOUT) {
			fprintf(stderr, "OUT: %s\n", libusb_error_name(r));
			break;
		}
		frames_sent += (uint64_t)frames_per_xfer;

		if (drain_in) {
			int got = 0;

			r = libusb_interrupt_transfer(h, 0x82, in_buf,
						      (int)in_len, &got, 10);
			/* Timeouts are normal. */
		}
	}

	printf("sent %" PRIu64 " frames\n", frames_sent);

	libusb_set_interface_alt_setting(h, iface, 0);
	libusb_release_interface(h, iface);
	libusb_close(h);
	free(out_buf);
	free(in_buf);
}

static void usage(const char *argv0)
{
	printf("usage: %s [--vid HEX] [--pid HEX] <command> [options]\n"
	       "\n"
	       "Commands:\n"
	       "  info                 dump descriptors, interfaces, endpoints\n"
	       "  fw                   read firmware version (vendor 0x1c)\n"
	       "  rate                 read current rate (vendor 0x11/0x10)\n"
	       "  status               read status (vendor 0x19, several lengths)\n"
	       "  raw <req> [len] [value] [index]\n"
	       "                       generic vendor IN request (read-only)\n"
	       "  write <req> [value] [index] --yes\n"
	       "                       generic vendor OUT request (0x40)\n"
	       "  bulk [options]       read a bulk/interrupt IN endpoint\n"
	       "      --iface N        default 1\n"
	       "      --alt N          default 0\n"
	       "      --ep HEX         default 0x85\n"
	       "      --len N          default 512\n"
	       "      --count N        default 4\n"
	       "      --timeout MS     default 500\n"
	       "      --out FILE       raw transfers (u32 len + data)\n"
	       "  set-rate --rate N [--dry-run] --yes\n"
	       "                       send the UC/UCX rate sequence (0x1b + 0x10)\n"
	       "  stream [options]     capture raw packets\n"
	       "      --iface 0|5      ISO (IF0) or interrupt (IF5), default 0\n"
	       "      --alt 0..3       alternate setting, default 1\n"
	       "      --seconds N      capture duration, default 2\n"
	       "      --out FILE       output file (RMEP record format)\n"
	       "      --hex N          hexdump the first N packets, default 4\n"
	       "  play [options]       send a 48 kHz tone to IF0 and capture IN\n"
	       "                       (default: format p, live-position offset 396)\n"
	       "      --seconds N      duration, default 3\n"
	       "      --tone HZ        tone frequency, default 1000\n"
	       "      --amp F          amplitude 0..1, default 0.1\n"
	       "      --channel N      first channel 0..17, default 0\n"
	       "      --format a|b|c|d|e|f|g|h|i|j|k|m|n|p (default p)\n"
	       "                         a=4+72N, b=76N+0x16, c=72N, d=72N+4,\n"
	       "                         e=76N+1/32ts, f=76N+1/32ts+0x4d80,\n"
	       "                         g=4+72N with 0x4d80 timestamp high word,\n"
	       "                         h=76N audio+trailer, i=4+72N zero header,\n"
	       "                         j=4+72N small counter, k=4+72N unshifted index,\n"
	       "                         m=Mac-mode 76N, first-frame 13-bit position\n"
	       "                         n=4+planar 72N with position header,\n"
	       "                         p=4+interleaved 72N with position header\n"
	       "      --sample-format S float|int32|int24, default float\n"
	       "      --use-in-ts      start OUT timestamp from a live IN packet\n"
	       "      --offset N       seed OUT position from live IN position + N (default 396)\n"
	       "      --capture FILE   IN capture, default cap-play-in.bin\n"
	       "      --no-capture     do not open the IN endpoint\n"
	       "      --no-unmute      skip the 0x14 unmute before streaming\n"
	       "      --hex N          hexdump the first N IN packets\n"
	       "  pcplay [options]     send a 48 kHz int32 tone to IF5 (RME PC mode)\n"
	       "      --seconds N      duration, default 3\n"
	       "      --frames N       frames per transfer, default 1024\n"
	       "      --amp F          amplitude 0..1, default 0.25\n"
	       "      --tone-l HZ      left tone, default 440\n"
	       "      --tone-r HZ      right tone, default 880\n"
	       "      --iface N        default 5\n"
	       "      --alt N          default 2\n"
	       "      --no-in          do not drain the IF5 IN endpoint\n"
	       "      --no-unmute      skip the 0x14 unmute\n"
	       "\n"
	       "Default device: 0424:3fc9 (Fireface UCX). Use --pid 0x3fc6 for UC.\n"
	       "Permission: sudo ./scripts/setup.sh  (installs the udev rule)\n",
	       argv0);
}

int main(int argc, char **argv)
{
	libusb_context *ctx = NULL;
	uint16_t vid = RME_VID_OLD;
	uint16_t pid = PID_UCX;
	int i = 1, r;

	signal(SIGINT, on_signal);
	signal(SIGTERM, on_signal);

	while (i < argc && argv[i][0] == '-' && argv[i][1] == '-') {
		if (!strcmp(argv[i], "--vid") && i + 1 < argc)
			vid = (uint16_t)strtoul(argv[++i], NULL, 0);
		else if (!strcmp(argv[i], "--pid") && i + 1 < argc)
			pid = (uint16_t)strtoul(argv[++i], NULL, 0);
		else if (!strcmp(argv[i], "--help") || !strcmp(argv[i], "-h")) {
			usage(argv[0]);
			return 0;
		} else
			die("unknown option: %s", argv[i]);
		i++;
	}

	if (i >= argc) {
		usage(argv[0]);
		return 1;
	}

	r = libusb_init(&ctx);
	if (r < 0)
		die("libusb_init: %s", libusb_error_name(r));
	libusb_set_option(ctx, LIBUSB_OPTION_LOG_LEVEL, LIBUSB_LOG_LEVEL_WARNING);

	const char *cmd = argv[i++];
	if (!strcmp(cmd, "info"))
		cmd_info(ctx, vid, pid);
	else if (!strcmp(cmd, "fw"))
		cmd_fw(ctx, vid, pid);
	else if (!strcmp(cmd, "rate"))
		cmd_rate(ctx, vid, pid);
	else if (!strcmp(cmd, "status"))
		cmd_status(ctx, vid, pid);
	else if (!strcmp(cmd, "raw"))
		cmd_raw(ctx, vid, pid, argc - i, argv + i);
	else if (!strcmp(cmd, "write"))
		cmd_write(ctx, vid, pid, argc - i, argv + i);
	else if (!strcmp(cmd, "bulk"))
		cmd_bulk(ctx, vid, pid, argc - i, argv + i);
	else if (!strcmp(cmd, "set-rate"))
		cmd_set_rate(ctx, vid, pid, argc - i, argv + i);
	else if (!strcmp(cmd, "stream"))
		cmd_stream(ctx, vid, pid, argc - i, argv + i);
	else if (!strcmp(cmd, "play"))
		cmd_play(ctx, vid, pid, argc - i, argv + i);
	else if (!strcmp(cmd, "pcplay"))
		cmd_pcplay(ctx, vid, pid, argc - i, argv + i);
	else if (!strcmp(cmd, "help") || !strcmp(cmd, "-h")) {
		usage(argv[0]);
		libusb_exit(ctx);
		return 0;
	} else {
		usage(argv[0]);
		libusb_exit(ctx);
		return 1;
	}

	libusb_exit(ctx);
	return 0;
}
