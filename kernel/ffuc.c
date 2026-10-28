// SPDX-License-Identifier: GPL-2.0-only
/*
 * RME Fireface USB - kernel transport core.
 *
 * This file owns USB enumeration, the streaming engines, the ALSA PCM
 * device and the per-device descriptor table.  Device-model policy lives in
 * userspace behind the snd-hwdep node created by ffuc-hwdep.c; see
 * ffuc.h and docs/PROTOCOL-COMPLETE.md.
 */
#include <linux/bitops.h>
#include <linux/debugfs.h>
#include <linux/ktime.h>
#include <linux/math64.h>
#include <linux/module.h>
#include <linux/slab.h>
#include <linux/string.h>
#include <linux/usb.h>
#include <sound/core.h>
#include <sound/initval.h>
#include <sound/pcm.h>
#include <sound/pcm_params.h>

#include "ffuc.h"

static int index[SNDRV_CARDS] = SNDRV_DEFAULT_IDX;
static char *id[SNDRV_CARDS] = SNDRV_DEFAULT_STR;
static bool enable[SNDRV_CARDS] = SNDRV_DEFAULT_ENABLE_PNP;
static unsigned int urb_frames = 48;
static unsigned int nurbs = 2;
static bool ucx_if5 = false;
static unsigned int ucx_if5_frames = 1024;
static unsigned int fixed_rate;

static DEFINE_MUTEX(devices_mutex);
static unsigned long devices_used;

static struct usb_driver ffuc_driver;

module_param_array(index, int, NULL, 0444);
MODULE_PARM_DESC(index, "Index value for the Fireface sound card.");
module_param_array(id, charp, NULL, 0444);
MODULE_PARM_DESC(id, "ID string for the Fireface sound card.");
module_param_array(enable, bool, NULL, 0444);
MODULE_PARM_DESC(enable, "Enable the Fireface sound card.");
module_param(urb_frames, uint, 0444);
MODULE_PARM_DESC(urb_frames, "Frames per URB at 32-48 kHz, multiple of 8 (8..256); doubled at 2x and quadrupled at 4x on the interrupt transport.");
module_param(nurbs, uint, 0444);
MODULE_PARM_DESC(nurbs, "URBs in flight per endpoint (2..32, default 2); raised so at least 64 frames stay queued.");
module_param(ucx_if5, bool, 0444);
MODULE_PARM_DESC(ucx_if5, "Fireface UCX: bind the Windows-style interrupt interface 5 (default false). The default binds the Mac-style ISO interface 0; set Y only for the Windows transport.");
module_param(ucx_if5_frames, uint, 0444);
MODULE_PARM_DESC(ucx_if5_frames, "Fireface UCX interrupt transport: frames per URB at 1x rates (8..1024, default 1024).");
module_param(fixed_rate, uint, 0644);
MODULE_PARM_DESC(fixed_rate, "Only allow this sample rate (0 = any supported rate).");

/* -- sample rates and bandwidth classes ----------------------------- */

static const struct ffuc_mode ffuc_pc_modes[] = {
	{ .alt = 1, .neps = 1, .words = 18, .pkt_frames = 8 },	/* 1x */
	{ .alt = 2, .neps = 1, .words = 14, .pkt_frames = 16 },	/* 2x */
	{ .alt = 3, .neps = 2, .words = 6, .pkt_frames = 32 },	/* 4x */
};

/* UCX interrupt transport (interface 5), as captured from the Windows
 * driver on the bench unit:
 *   - alt 1 (576 B) accepts 48 kHz but is SILENT at the DAC;
 *   - alt 2 (896 B) is the working 1x/2x path;
 *   - alt 3 (4 x 768 B) is the 4x path.
 * 1x keeps 18 words/frame; 2x uses 14 (ADAT SMUX).  The old driver used
 * 18 at 2x, untested; the descriptor arithmetic says 14.  See
 * docs/PROTOCOL-COMPLETE.md TODO #11.
 */
static const struct ffuc_mode ffuc_ucx_if5_modes[] = {
	{ .alt = 2, .neps = 1, .words = 18, .pkt_frames = 8 },
	{ .alt = 2, .neps = 1, .words = 14, .pkt_frames = 16 },
	{ .alt = 3, .neps = 2, .words = 6, .pkt_frames = 32 },
};

/* ISO transport (UC Mac personality and UCX interface 0).  The packet
 * maxima are wMaxPacketSize * mult from the descriptors; the UCX OUT
 * packet adds one lead word per frame. */
static const struct ffuc_mode ffuc_mac_modes[] = {
	{ .alt = 1, .neps = 1, .words = 18, .in_mps = 508, .out_mps = 532, .max_frames = 7 },
	{ .alt = 2, .neps = 1, .words = 14, .in_mps = 732, .out_mps = 780, .max_frames = 13 },
	{ .alt = 3, .neps = 1, .words = 12, .in_mps = 1208, .out_mps = 1304, .max_frames = 25 },
};

static const struct ffuc_mode ffuc_ucx_modes[] = {
	{ .alt = 1, .neps = 1, .words = 18, .in_mps = 508, .out_mps = 532, .max_frames = 7 },
	{ .alt = 2, .neps = 1, .words = 14, .in_mps = 732, .out_mps = 780, .max_frames = 13 },
	{ .alt = 3, .neps = 1, .words = 12, .in_mps = 1208, .out_mps = 1304, .max_frames = 25 },
};

static const unsigned int ffuc_rates[] = {
	32000, 44100, 48000, 64000, 88200, 96000, 128000, 176400, 192000,
	352800, 384000, 705600, 768000,
};

static const struct snd_pcm_hw_constraint_list ffuc_rate_list = {
	.count = ARRAY_SIZE(ffuc_rates),
	.list = ffuc_rates,
};

static unsigned int ffuc_mode_index(unsigned int rate)
{
	if (rate <= 48000)
		return 0;
	if (rate <= 96000)
		return 1;
	return 2;
}

static bool ffuc_rate_supported(struct ffuc *chip, unsigned int rate)
{
	int i;

	if (rate > chip->desc->max_rate)
		return false;
	for (i = 0; i < ARRAY_SIZE(ffuc_rates); i++)
		if (ffuc_rates[i] == rate)
			return true;
	return false;
}

static unsigned int ffuc_mode_channels(struct ffuc *chip, unsigned int rate)
{
	const struct ffuc_mode *m = &chip->modes[ffuc_mode_index(rate)];

	return m->neps * m->words;
}

/*
 * The ALSA PCM keeps a constant logical channel count.  At 2x/4x the USB
 * packet carries fewer words (ADAT S/MUX); the engine maps the first
 * 14/12 words and the remaining channels stay silent.  This lets PipeWire
 * keep one 18-channel node while the card runs at every supported rate.
 */
static unsigned int ffuc_max_channels(struct ffuc *chip, unsigned int rate)
{
	(void)rate;
	return chip->desc->max_channels;
}

static unsigned int ffuc_max_words(struct ffuc *chip)
{
	unsigned int i, n = 0;

	for (i = 0; i < chip->desc->n_modes; i++)
		n = max(n, chip->modes[i].neps * chip->modes[i].words);
	return n;
}

static unsigned int ffuc_urb_frames(struct ffuc *chip, unsigned int rate)
{
	if (chip->iso)
		return chip->iso_packets * DIV_ROUND_UP(rate, 8000);
	/* The Windows IF5 driver uses one fixed URB size at every rate. */
	if (chip->ucx)
		return chip->base_urb_frames;
	return chip->base_urb_frames << ffuc_mode_index(rate);
}

static unsigned int ffuc_iso_wrap(struct ffuc *chip)
{
	/* The UCX counts 12 bits (wrap 3072); the UC Mac path was captured
	 * with a 13-bit position (wrap 8192). */
	return chip->ucx ? 3072 : FFUC_ISO_COUNTER_MOD;
}

unsigned int ffuc_rate_from_index(unsigned int idx)
{
	static const unsigned int map[12] = {
		32000, 44100, 48000, 48000, 64000, 88200,
		96000, 96000, 128000, 176400, 192000, 192000,
	};

	return idx < ARRAY_SIZE(map) ? map[idx] : 0;
}

/* -- vendor requests ------------------------------------------------- */

int ffuc_vendor_send(struct ffuc *chip, u8 req, u16 val, u16 idx,
		     const void *data, u16 len)
{
	if (READ_ONCE(chip->disconnected))
		return -ENODEV;
	return usb_control_msg_send(chip->dev, 0, req,
				    USB_DIR_OUT | USB_TYPE_VENDOR |
				    USB_RECIP_DEVICE, val, idx,
				    (void *)data, len, FFUC_CTL_TIMEOUT,
				    GFP_KERNEL);
}

int ffuc_vendor_recv(struct ffuc *chip, u8 req, u16 val, u16 idx,
		     void *data, u16 len)
{
	if (READ_ONCE(chip->disconnected))
		return -ENODEV;
	return usb_control_msg_recv(chip->dev, 0, req,
				    USB_DIR_IN | USB_TYPE_VENDOR |
				    USB_RECIP_DEVICE, val, idx,
				    data, len, FFUC_CTL_TIMEOUT, GFP_KERNEL);
}

/* One IF1 register word: [parity:1][addr:15][value:16], little-endian. */
static u32 ffuc_word_encode(u16 addr, u16 value)
{
	u32 w = ((u32)(addr & 0x7fff) << 16) | value;
	u32 p = w;

	p ^= p >> 16;
	p ^= p >> 8;
	p ^= p >> 4;
	p ^= p >> 2;
	p ^= p >> 1;
	if (!(p & 1))
		w |= 0x80000000u;
	return w;
}

int ffuc_reg_send(struct ffuc *chip, const struct ffuc_hwdep_word *words,
		  unsigned int count)
{
	u8 *buf;
	unsigned int done = 0;
	int ret = 0;

	if (!chip->desc->state_out_ep)
		return -ENODEV;
	if (!count)
		return 0;
	if (count > FFUC_HWDEP_MAX_WORDS)
		count = FFUC_HWDEP_MAX_WORDS;

	buf = kmalloc(count * sizeof(u32), GFP_KERNEL);
	if (!buf)
		return -ENOMEM;

	while (done < count) {
		unsigned int n = min_t(unsigned int, count - done,
				       FFUC_HWDEP_MAX_WORDS);
		unsigned int i, actual;
		u8 *p = buf;

		for (i = 0; i < n; i++) {
			u32 w = ffuc_word_encode(words[done + i].addr,
						 words[done + i].value);

			*p++ = w;
			*p++ = w >> 8;
			*p++ = w >> 16;
			*p++ = w >> 24;
		}
		mutex_lock(&chip->ctl_lock);
		ret = usb_bulk_msg(chip->dev,
				   usb_sndbulkpipe(chip->dev,
						   chip->desc->state_out_ep),
				   buf, n * sizeof(u32), &actual, 100);
		mutex_unlock(&chip->ctl_lock);
		if (ret < 0)
			break;
		done += n;
	}
	kfree(buf);
	return ret;
}

/* Claim/release a sibling interface for the raw control transports.  The
 * USB driver itself only probes the streaming interface, so the meter/state
 * interface must be claimed explicitly.  The helpers keep ffuc_driver local
 * to this file while ffuc-hwdep.c can still manage the interface. */
int ffuc_claim_interface(struct usb_interface *intf)
{
	return usb_driver_claim_interface(&ffuc_driver, intf, NULL);
}

void ffuc_release_interface(struct usb_interface *intf)
{
	usb_driver_release_interface(&ffuc_driver, intf);
}

/* -- per-device core sequences --------------------------------------- */

static int ffuc_uc_init(struct ffuc *chip)
{
	int i, ret;

	for (i = 0; i < FFUC_MAX_CHANNELS; i++) {
		ret = ffuc_vendor_send(chip, FFUC_REQ_CHAN_INIT, 0x0001, i,
				       NULL, 0);
		if (ret < 0)
			return ret;
		ret = ffuc_vendor_send(chip, FFUC_REQ_CHAN_INIT, 0x0001,
				       i + 0x20, NULL, 0);
		if (ret < 0)
			return ret;
	}
	ret = ffuc_vendor_send(chip, FFUC_REQ_SETTINGS, 0x0800, 0xb800,
			       NULL, 0);
	if (ret < 0)
		return ret;
	return ffuc_vendor_send(chip, FFUC_REQ_ENABLE, 0x0001, 0x0000,
				NULL, 0);
}

static unsigned int ffuc_rate_family(unsigned int rate)
{
	switch (rate) {
	case 32000:
	case 64000:
	case 128000:
		return 0;
	case 44100:
	case 88200:
	case 176400:
		return 1;
	default:
		return 2;
	}
}

static int ffuc_set_rate_family(struct ffuc *chip, unsigned int rate)
{
	return ffuc_vendor_send(chip, FFUC_REQ_SETTINGS,
				ffuc_rate_family(rate) << 4, 0x0030, NULL, 0);
}

static int ffuc_uc_set_rate(struct ffuc *chip, unsigned int rate)
{
	u32 dds;
	int ret;

	dev_info(&chip->dev->dev, "UC rate -> %u Hz (iso=%d)\n", rate,
		 chip->iso);
	/* Windows UC PC uses only 0x10/48 (family) + 0x10/1487=1 (commit).
	 * The DDS word is needed by the ISO/Mac personality, not the
	 * interrupt/Windows one; sending it there can override the committed
	 * family and leave the device silent. */
	if (chip->iso) {
		dds = (u32)div_u64(0x5f5e10000000ULL, rate);
		if (rate > 114000)
			dds <<= 2;
		else if (rate > 57000)
			dds <<= 1;

		ret = ffuc_vendor_send(chip, FFUC_REQ_DDS, dds >> 16,
				       (dds & 0xff00) | ffuc_rate_family(rate),
				       NULL, 0);
		dev_info(&chip->dev->dev,
			 "UC DDS 0x%04x/0x%04x ret=%d\n", dds >> 16,
			 (dds & 0xff00) | ffuc_rate_family(rate), ret);
		if (ret < 0)
			return ret;
	}
	ret = ffuc_set_rate_family(chip, rate);
	dev_info(&chip->dev->dev,
		 "UC rate family 0x%02x index 48 ret=%d\n",
		 ffuc_rate_family(rate) << 4, ret);
	if (ret < 0)
		return ret;
	/* Windows commits every rate change with 0x10 / wIndex 1487 /
	 * wValue 1.  Without this the new family is written but not applied;
	 * the stream can run while the device output stays closed. */
	ret = ffuc_vendor_send(chip, FFUC_REQ_SETTINGS, 0x0001, 1487,
			       NULL, 0);
	dev_info(&chip->dev->dev, "UC rate commit 0x0001/1487 ret=%d\n", ret);
	return ret;
}

/* UC Mac/personality and UCX: rate family is a masked control write.
 * The interrupt transport additionally takes the vendor DDS word. */
static int ffuc_ucx_set_rate(struct ffuc *chip, unsigned int rate)
{
	if (!chip->iso)
		return ffuc_uc_set_rate(chip, rate);
	return ffuc_set_rate_family(chip, rate);
}

static int ffuc_uc_hw_mute(struct ffuc *chip, bool mute)
{
	return ffuc_vendor_send(chip,
				mute ? FFUC_REQ_MUTE : FFUC_REQ_UNMUTE,
				0xffff, 0xc000, NULL, 0);
}

/* Requests 19/20 mute/unmute playback channels by mask: channel n is bit
 * n + 2, high half in wValue, low half in wIndex. */
static int ffuc_playback_mask(struct ffuc *chip, bool mute,
			      unsigned int channels)
{
	u32 mask;
	u16 idx, val;

	if (channels > 30)
		channels = 30;
	mask = ((1u << channels) - 1) << 2;
	val = mask >> 16;
	idx = mask & 0xffff;
	return ffuc_vendor_send(chip,
				mute ? FFUC_REQ_UCX_MUTE :
				       FFUC_REQ_UCX_UNMUTE,
				val, idx, NULL, 0);
}

static int ffuc_ucx_hw_mute(struct ffuc *chip, bool mute)
{
	return ffuc_playback_mask(chip, mute, FFUC_MAX_CHANNELS);
}

static int ffuc_ucx_stream_start(struct ffuc *chip, unsigned int channels)
{
	int ret;

	/* The Windows driver sends these immediately before the first
	 * audio packet.  0x8000/0x8000 is required for the DACs to open on
	 * the interrupt transport; 0x1d is not understood but is part of the
	 * captured working sequence. */
	ret = ffuc_vendor_send(chip, FFUC_REQ_SETTINGS, 0x8000, 0x8000,
			       NULL, 0);
	if (ret < 0)
		return ret;
	ret = ffuc_vendor_send(chip, FFUC_REQ_UCX_STREAM29, 0x0000, 0x0000,
			       NULL, 0);
	if (ret < 0)
		return ret;
	return ffuc_playback_mask(chip, false, channels);
}

static int ffuc_ucx_stream_stop(struct ffuc *chip, unsigned int channels)
{
	return ffuc_playback_mask(chip, true, channels);
}

static int ffuc_uc_stream_start(struct ffuc *chip, unsigned int channels)
{
	int ret;

	(void)channels;
	if (!chip->uc_session)
		return 0;
	dev_info(&chip->dev->dev,
		 "UC session open (rate=%u channels=%u session_reopen=%d)\n",
		 chip->rate, channels, chip->session_reopen);

	/* Windows session-open sequence, measured in A2b on every stream
	 * open:
	 *   0x10  wIndex 0x8000  wValue 0x0000
	 *   0x1D  wIndex 0       wValue 0x0000
	 *   0x14  wIndex 0xC000  wValue 0x000F
	 * The UC needs it after a rate change; the initial 48 kHz path uses
	 * the probe/init sequence and must stay untouched.  Do not confuse
	 * wValue and wIndex: 0x10 carries 0x8000 in wIndex and zero in
	 * wValue. */
	ret = ffuc_vendor_send(chip, FFUC_REQ_SETTINGS, 0x0000, 0x8000,
			       NULL, 0);
	if (ret < 0)
		return ret;
	ret = ffuc_vendor_send(chip, FFUC_REQ_UCX_STREAM29, 0x0000, 0x0000,
			       NULL, 0);
	if (ret < 0)
		return ret;
	ret = ffuc_vendor_send(chip, FFUC_REQ_UNMUTE, 0x000f, 0xc000,
			       NULL, 0);
	if (ret < 0)
		return ret;
	chip->session_reopen = false;
	return 0;
}

static int ffuc_uc_stream_stop(struct ffuc *chip, unsigned int channels)
{
	(void)channels;
	/* Windows closes the session before every rate write (A2 t=13.03,
	 * rate at 13.25; FP9c t=4.88, rate at 10.48).  This is not gated on
	 * a previous rate change: the first 48 -> 44.1 switch needs it. */
	dev_info(&chip->dev->dev,
		 "UC session close (rate=%u channels=%u)\n",
		 chip->rate, channels);
	/* Windows session-close word: 0x13 wIndex 0xC000 wValue 0x000F. */
	return ffuc_vendor_send(chip, FFUC_REQ_MUTE, 0x000f, 0xc000,
				NULL, 0);
}

static const struct ffuc_ops ffuc_uc_ops = {
	.init = ffuc_uc_init,
	.set_rate = ffuc_uc_set_rate,
	.stream_start = ffuc_uc_stream_start,
	.stream_stop = ffuc_uc_stream_stop,
	.hw_mute = ffuc_uc_hw_mute,
};

static const struct ffuc_ops ffuc_ucx_ops = {
	.init = ffuc_uc_init,
	.set_rate = ffuc_ucx_set_rate,
	.stream_start = ffuc_ucx_stream_start,
	.stream_stop = ffuc_ucx_stream_stop,
	.hw_mute = ffuc_ucx_hw_mute,
};

/* -- per-device descriptors ------------------------------------------ */

static const struct ffuc_device_desc ffuc_devices[] = {
	{
		.name = "Fireface UC",
		.shortname = "Fireface UC",
		.card_id = "FirefaceUC",
		.vendor = FFUC_VENDOR_SMSC,
		.product = FFUC_PRODUCT_UC_PC,
		.iface = 0,
		.transport = FFUC_XPORT_INTERRUPT,
		.quirks = FFUC_QUIRK_STATUS_LATCH,
		.model = FFUC_MODEL_UC,
		.modes = ffuc_pc_modes,
		.n_modes = ARRAY_SIZE(ffuc_pc_modes),
		.audio_iface = 0,
		.out_ep = 0x01,
		.in_ep = 0x02,
		.status_iface = 5,
		.status_ep = 0x8b,
		.state_iface = 1,
		.state_out_ep = 0x0c,
		.state_in_ep = 0x8d,
		.meter_in_ep = 0x85,
		.max_channels = 18,
		.max_rate = 192000,
		.meter_len = 324,
		.ops = &ffuc_uc_ops,
	}, {
		.name = "Fireface UC",
		.shortname = "Fireface UC",
		.card_id = "FirefaceUC",
		.vendor = FFUC_VENDOR_SMSC,
		.product = FFUC_PRODUCT_UC_MAC,
		.iface = 0,
		.transport = FFUC_XPORT_ISO,
		.quirks = 0,
		.model = FFUC_MODEL_UC,
		.modes = ffuc_mac_modes,
		.n_modes = ARRAY_SIZE(ffuc_mac_modes),
		.audio_iface = 0,
		.out_ep = 0x01,
		.in_ep = 0x02,
		.status_iface = FFUC_IFACE_NONE,
		.status_ep = 0x8b,
		.state_iface = 1,
		.state_out_ep = 0x0c,
		.state_in_ep = 0x8d,
		.meter_in_ep = 0x85,
		.max_channels = 18,
		.max_rate = 192000,
		.meter_len = 324,
		.ops = &ffuc_uc_ops,
	}, {
		.name = "Fireface UC",
		.shortname = "Fireface UC",
		.card_id = "FirefaceUC",
		.vendor = FFUC_VENDOR_RME,
		.product = FFUC_PRODUCT_UC_MAC,
		.iface = 0,
		.transport = FFUC_XPORT_ISO,
		.quirks = 0,
		.model = FFUC_MODEL_UC,
		.modes = ffuc_mac_modes,
		.n_modes = ARRAY_SIZE(ffuc_mac_modes),
		.audio_iface = 0,
		.out_ep = 0x01,
		.in_ep = 0x02,
		.status_iface = FFUC_IFACE_NONE,
		.status_ep = 0x8b,
		.state_iface = 1,
		.state_out_ep = 0x0c,
		.state_in_ep = 0x8d,
		.meter_in_ep = 0x85,
		.max_channels = 18,
		.max_rate = 192000,
		.meter_len = 324,
		.ops = &ffuc_uc_ops,
	}, {
		.name = "Fireface UCX",
		.shortname = "Fireface UCX",
		.card_id = "FirefaceUCX",
		.vendor = FFUC_VENDOR_SMSC,
		.product = FFUC_PRODUCT_UCX,
		.iface = 0,
		.transport = FFUC_XPORT_ISO,
		.quirks = 0,
		.model = FFUC_MODEL_UCX,
		.modes = ffuc_ucx_modes,
		.n_modes = ARRAY_SIZE(ffuc_ucx_modes),
		.audio_iface = 0,
		.out_ep = 0x0e,
		.in_ep = 0x0f,
		.status_iface = 4,
		.status_ep = 0x8b,
		.state_iface = 1,
		.state_out_ep = 0x0c,
		.state_in_ep = 0x8d,
		.meter_in_ep = 0x85,
		.midi_iface = 2,
		.midi_out_ep = 0x07,
		.midi_in_ep = 0x86,
		.max_channels = 18,
		.max_rate = 192000,
		.meter_len = 244,
		.ops = &ffuc_ucx_ops,
	}, {
		.name = "Fireface UCX",
		.shortname = "Fireface UCX",
		.card_id = "FirefaceUCX",
		.vendor = FFUC_VENDOR_SMSC,
		.product = FFUC_PRODUCT_UCX,
		.iface = 5,
		.transport = FFUC_XPORT_INTERRUPT,
		.quirks = FFUC_QUIRK_ALT_RESELECT |
			  FFUC_QUIRK_INT32_SAMPLES |
			  FFUC_QUIRK_IF5_PRE_START |
			  FFUC_QUIRK_IF5_PLAYBACK_MASK,
		.model = FFUC_MODEL_UCX,
		.modes = ffuc_ucx_if5_modes,
		.n_modes = ARRAY_SIZE(ffuc_ucx_if5_modes),
		.audio_iface = 5,
		.out_ep = 0x01,
		.in_ep = 0x02,
		.status_iface = 4,
		.status_ep = 0x8b,
		.state_iface = 1,
		.state_out_ep = 0x0c,
		.state_in_ep = 0x8d,
		.meter_in_ep = 0x85,
		.midi_iface = 2,
		.midi_out_ep = 0x07,
		.midi_in_ep = 0x86,
		.max_channels = 18,
		.max_rate = 192000,
		.meter_len = 244,
		.ops = &ffuc_ucx_ops,
	}, {
		.name = "Fireface UCX",
		.shortname = "Fireface UCX",
		.card_id = "FirefaceUCX",
		.vendor = FFUC_VENDOR_RME,
		.product = FFUC_PRODUCT_UCX,
		.iface = 0,
		.transport = FFUC_XPORT_ISO,
		.quirks = 0,
		.model = FFUC_MODEL_UCX,
		.modes = ffuc_ucx_modes,
		.n_modes = ARRAY_SIZE(ffuc_ucx_modes),
		.audio_iface = 0,
		.out_ep = 0x0e,
		.in_ep = 0x0f,
		.status_iface = 4,
		.status_ep = 0x8b,
		.state_iface = 1,
		.state_out_ep = 0x0c,
		.state_in_ep = 0x8d,
		.meter_in_ep = 0x85,
		.midi_iface = 2,
		.midi_out_ep = 0x07,
		.midi_in_ep = 0x86,
		.max_channels = 18,
		.max_rate = 192000,
		.meter_len = 244,
		.ops = &ffuc_ucx_ops,
	}, {
		.name = "Fireface UCX",
		.shortname = "Fireface UCX",
		.card_id = "FirefaceUCX",
		.vendor = FFUC_VENDOR_RME,
		.product = FFUC_PRODUCT_UCX,
		.iface = 5,
		.transport = FFUC_XPORT_INTERRUPT,
		.quirks = FFUC_QUIRK_ALT_RESELECT |
			  FFUC_QUIRK_INT32_SAMPLES |
			  FFUC_QUIRK_IF5_PRE_START |
			  FFUC_QUIRK_IF5_PLAYBACK_MASK,
		.model = FFUC_MODEL_UCX,
		.modes = ffuc_ucx_if5_modes,
		.n_modes = ARRAY_SIZE(ffuc_ucx_if5_modes),
		.audio_iface = 5,
		.out_ep = 0x01,
		.in_ep = 0x02,
		.status_iface = 4,
		.status_ep = 0x8b,
		.state_iface = 1,
		.state_out_ep = 0x0c,
		.state_in_ep = 0x8d,
		.meter_in_ep = 0x85,
		.midi_iface = 2,
		.midi_out_ep = 0x07,
		.midi_in_ep = 0x86,
		.max_channels = 18,
		.max_rate = 192000,
		.meter_len = 244,
		.ops = &ffuc_ucx_ops,
	},
};

static const struct usb_device_id ffuc_ids[] = {
	{ USB_DEVICE_INTERFACE_NUMBER(FFUC_VENDOR_SMSC, FFUC_PRODUCT_UC_PC, 0),
	  .driver_info = (kernel_ulong_t)&ffuc_devices[0] },
	{ USB_DEVICE_INTERFACE_NUMBER(FFUC_VENDOR_SMSC, FFUC_PRODUCT_UC_MAC, 0),
	  .driver_info = (kernel_ulong_t)&ffuc_devices[1] },
	{ USB_DEVICE_INTERFACE_NUMBER(FFUC_VENDOR_RME, FFUC_PRODUCT_UC_MAC, 0),
	  .driver_info = (kernel_ulong_t)&ffuc_devices[2] },
	{ USB_DEVICE_INTERFACE_NUMBER(FFUC_VENDOR_SMSC, FFUC_PRODUCT_UCX, 0),
	  .driver_info = (kernel_ulong_t)&ffuc_devices[3] },
	{ USB_DEVICE_INTERFACE_NUMBER(FFUC_VENDOR_SMSC, FFUC_PRODUCT_UCX, 5),
	  .driver_info = (kernel_ulong_t)&ffuc_devices[4] },
	{ USB_DEVICE_INTERFACE_NUMBER(FFUC_VENDOR_RME, FFUC_PRODUCT_UCX, 0),
	  .driver_info = (kernel_ulong_t)&ffuc_devices[5] },
	{ USB_DEVICE_INTERFACE_NUMBER(FFUC_VENDOR_RME, FFUC_PRODUCT_UCX, 5),
	  .driver_info = (kernel_ulong_t)&ffuc_devices[6] },
	{ }
};
MODULE_DEVICE_TABLE(usb, ffuc_ids);

/* -- sample format conversion --------------------------------------- */

static u32 ffuc_s32_to_f32(s32 v)
{
	u32 sign = 0, mag, mant;
	int e;

	if (!v)
		return 0;
	if (v < 0) {
		sign = 0x80000000;
		mag = -(u32)v;
	} else {
		mag = v;
	}
	e = fls(mag) - 1;
	if (e > 23)
		mant = mag >> (e - 23);
	else
		mant = mag << (23 - e);
	return sign | ((u32)(e - 31 + 127) << 23) | (mant & 0x7fffff);
}

static s32 ffuc_f32_to_s32(u32 f)
{
	u32 exp = (f >> 23) & 0xff;
	u32 mant = (f & 0x7fffff) | 0x800000;
	bool neg = f >> 31;
	int shift;
	u32 mag;

	if (exp == 0xff && (f & 0x7fffff))
		return 0;
	shift = (int)exp - 119;
	if (shift >= 8)
		return neg ? S32_MIN : S32_MAX;
	if (shift >= 0)
		mag = mant << shift;
	else if (shift > -24)
		mag = mant >> -shift;
	else
		return 0;
	return neg ? -(s32)mag : (s32)mag;
}

/* -- streaming engine ------------------------------------------------ */

/*
 * At 4x the interrupt transport has two capture endpoints: endpoint 0
 * carries words 0..words-1, endpoint 1 carries the next slice.  Each
 * endpoint must only zero channels past the *total* active word count;
 * clearing past its own slice would wipe the other endpoint's channels
 * whenever endpoint 0 completes after endpoint 1.
 */
static void ffuc_capture_copy(struct snd_pcm_runtime *rt, snd_pcm_uframes_t pos,
			      const __le32 *src, unsigned int frames,
			      unsigned int nch, unsigned int coff,
			      unsigned int fstride, unsigned int cstride,
			      unsigned int total)
{
	bool is_float = rt->format == SNDRV_PCM_FORMAT_FLOAT_LE;
	unsigned int chans = rt->channels;
	unsigned int p = pos % rt->buffer_size;
	unsigned int f, c;

	for (f = 0; f < frames; f++, src += fstride) {
		u32 *dst = (u32 *)rt->dma_area + p * chans;

		for (c = 0; c < nch && coff + c < chans; c++) {
			u32 v = le32_to_cpu(src[c * cstride]);

			dst[coff + c] = is_float ? v : (u32)ffuc_f32_to_s32(v);
		}
		/* 2x/4x packets carry only 14/12 words; keep the logical
		 * 18-channel ALSA buffer silent on the unused ADAT slots.
		 * Only clear channels that no endpoint fills. */
		for (; c + coff < chans; c++)
			if (coff + c >= total)
				dst[coff + c] = 0;
		if (++p == rt->buffer_size)
			p = 0;
	}
}

static void ffuc_capture_copy_int32(struct snd_pcm_runtime *rt,
				    snd_pcm_uframes_t pos,
				    const __le32 *src, unsigned int frames,
				    unsigned int nch, unsigned int coff,
				    unsigned int fstride, unsigned int cstride,
				    unsigned int total)
{
	bool is_float = rt->format == SNDRV_PCM_FORMAT_FLOAT_LE;
	unsigned int chans = rt->channels;
	unsigned int p = pos % rt->buffer_size;
	unsigned int f, c;

	for (f = 0; f < frames; f++, src += fstride) {
		u32 *dst = (u32 *)rt->dma_area + p * chans;

		for (c = 0; c < nch && coff + c < chans; c++) {
			u32 v = le32_to_cpu(src[c * cstride]);

			dst[coff + c] = is_float ? ffuc_s32_to_f32((s32)v) : v;
		}
		for (; c + coff < chans; c++)
			if (coff + c >= total)
				dst[coff + c] = 0;
		if (++p == rt->buffer_size)
			p = 0;
	}
}

static void ffuc_playback_copy(struct snd_pcm_runtime *rt, snd_pcm_uframes_t pos,
			       __le32 *dst, unsigned int frames,
			       unsigned int nch, unsigned int coff,
			       unsigned int fstride)
{
	bool is_float = rt->format == SNDRV_PCM_FORMAT_FLOAT_LE;
	unsigned int chans = rt->channels;
	unsigned int p = pos % rt->buffer_size;
	unsigned int f, c;

	for (f = 0; f < frames; f++, dst += fstride) {
		const u32 *src = (const u32 *)rt->dma_area + p * chans;

		for (c = 0; c < nch; c++) {
			u32 v = 0;

			if (coff + c < chans)
				v = is_float ? src[coff + c] :
				    ffuc_s32_to_f32((s32)src[coff + c]);
			dst[c] = cpu_to_le32(v);
		}
		if (++p == rt->buffer_size)
			p = 0;
	}
}

static void ffuc_playback_copy_int32(struct snd_pcm_runtime *rt,
				     snd_pcm_uframes_t pos, __le32 *dst,
				     unsigned int frames, unsigned int nch,
				     unsigned int coff, unsigned int fstride)
{
	bool is_float = rt->format == SNDRV_PCM_FORMAT_FLOAT_LE;
	unsigned int chans = rt->channels;
	unsigned int p = pos % rt->buffer_size;
	unsigned int f, c;

	for (f = 0; f < frames; f++, dst += fstride) {
		const u32 *src = (const u32 *)rt->dma_area + p * chans;

		for (c = 0; c < nch; c++) {
			u32 v = 0;

			if (coff + c < chans)
				v = is_float ?
				    (u32)ffuc_f32_to_s32(src[coff + c]) :
				    src[coff + c];
			dst[c] = cpu_to_le32(v);
		}
		if (++p == rt->buffer_size)
			p = 0;
	}
}

static bool ffuc_stream_advance(struct ffuc *chip, struct ffuc_stream *s,
				struct snd_pcm_runtime *rt, unsigned int idx,
				unsigned int frames)
{
	snd_pcm_uframes_t hw;
	int i;

	s->pos[idx] += frames;
	hw = s->pos[0];
	for (i = 1; i < chip->mode->neps; i++)
		hw = min(hw, s->pos[i]);
	s->hw_ptr = hw;
	if (hw / rt->period_size == s->period)
		return false;
	s->period = hw / rt->period_size;
	return true;
}

/* Interrupt transport: one URB of interleaved frames. */
static void ffuc_capture_urb(struct ffuc_ep *ep, struct urb *urb)
{
	struct ffuc *chip = ep->chip;
	struct ffuc_stream *s = &chip->streams[SNDRV_PCM_STREAM_CAPTURE];
	struct snd_pcm_substream *subs = s->substream;
	const unsigned int words = chip->mode->words;
	const unsigned int total = chip->mode->neps * words;
	unsigned int frames = urb->actual_length / (words * FFUC_SAMPLE_BYTES);
	unsigned long flags;

	if (!frames)
		return;

	snd_pcm_stream_lock_irqsave(subs, flags);
	if (s->running) {
		if (chip->desc->quirks & FFUC_QUIRK_INT32_SAMPLES)
			ffuc_capture_copy_int32(subs->runtime, s->pos[ep->index],
						urb->transfer_buffer, frames, words,
						ep->index * words, words, 1, total);
		else
			ffuc_capture_copy(subs->runtime, s->pos[ep->index],
					  urb->transfer_buffer, frames, words,
					  ep->index * words, words, 1, total);
		if (ffuc_stream_advance(chip, s, subs->runtime, ep->index, frames))
			snd_pcm_period_elapsed_under_stream_lock(subs);
	}
	snd_pcm_stream_unlock_irqrestore(subs, flags);
}

static void ffuc_playback_urb(struct ffuc_ep *ep, struct urb *urb)
{
	struct ffuc *chip = ep->chip;
	struct ffuc_stream *s = &chip->streams[SNDRV_PCM_STREAM_PLAYBACK];
	struct snd_pcm_substream *subs = s->substream;
	const unsigned int words = chip->mode->words;
	const unsigned int frames = chip->urb_frames;
	unsigned long flags;

	snd_pcm_stream_lock_irqsave(subs, flags);
	if (s->running) {
		if (chip->desc->quirks & FFUC_QUIRK_INT32_SAMPLES)
			ffuc_playback_copy_int32(subs->runtime, s->pos[ep->index],
						 urb->transfer_buffer, frames,
						 words, ep->index * words,
						 words);
		else
			ffuc_playback_copy(subs->runtime, s->pos[ep->index],
					   urb->transfer_buffer, frames, words,
					   ep->index * words, words);
		if (ffuc_stream_advance(chip, s, subs->runtime, ep->index, frames))
			snd_pcm_period_elapsed_under_stream_lock(subs);
	} else {
		memset(urb->transfer_buffer, 0, urb->transfer_buffer_length);
	}
	snd_pcm_stream_unlock_irqrestore(subs, flags);
}

/* ISO implicit feedback: remember every IN packet's position and frame
 * count by packet number. */
static void ffuc_fb_put(struct ffuc *chip, unsigned int pos, unsigned int frames,
			unsigned int phase)
{
	u32 e;

	if (chip->ucx)
		e = ((phase & 0x7ff) << 21) | ((pos & 0xfff) << 5) |
		    (frames & 0x1f);
	else
		e = ((phase & 0x3fff) << 18) | ((pos & 0x1fff) << 5) |
		    (frames & 0x1f);
	chip->fb[chip->in_pkts++ & (FFUC_FB_SIZE - 1)] = e;
	chip->in_end = (pos + frames) % ffuc_iso_wrap(chip);
}

static void ffuc_fb_put_nominal(struct ffuc *chip)
{
	chip->fb_acc += chip->rate;
	ffuc_fb_put(chip, chip->in_end, chip->fb_acc / 8000, 0);
	chip->fb_acc %= 8000;
}

static bool ffuc_fb_get(struct ffuc *chip, u64 k, unsigned int *pos,
			unsigned int *frames, unsigned int *phase)
{
	u64 src = k - chip->fb_delay;
	u32 e;

	if (k < chip->fb_delay || src >= chip->in_pkts ||
	    chip->in_pkts - src > FFUC_FB_SIZE)
		return false;
	e = chip->fb[src & (FFUC_FB_SIZE - 1)];
	if (chip->ucx) {
		*pos = (e >> 5) & 0xfff;
		*phase = e >> 21;
	} else {
		*pos = (e >> 5) & 0x1fff;
		*phase = (e >> 18) & 0x3fff;
	}
	*frames = min(e & 0x1f, chip->mode->max_frames);
	return true;
}

static void ffuc_iso_capture_urb(struct ffuc_ep *ep, struct urb *urb)
{
	struct ffuc *chip = ep->chip;
	struct ffuc_stream *s = &chip->streams[SNDRV_PCM_STREAM_CAPTURE];
	struct snd_pcm_substream *subs = s->substream;
	const struct ffuc_mode *m = chip->mode;
	const unsigned int nch = m->words;
	unsigned int frames[FFUC_MAX_ISO_PACKETS];
	unsigned long flags;
	bool elapsed = false;
	int i;

	spin_lock_irqsave(&chip->fb_lock, flags);
	for (i = 0; i < urb->number_of_packets; i++) {
		const struct usb_iso_packet_descriptor *d =
			&urb->iso_frame_desc[i];
		const u8 *pkt = (const u8 *)urb->transfer_buffer + d->offset;
		u32 hdr;

		if (d->status || d->actual_length < FFUC_ISO_IN_HEADER ||
		    (d->actual_length - FFUC_ISO_IN_HEADER) %
		    (nch * FFUC_SAMPLE_BYTES)) {
			frames[i] = 0;
			ffuc_fb_put_nominal(chip);
			continue;
		}
		frames[i] = min((d->actual_length - FFUC_ISO_IN_HEADER) /
				(nch * FFUC_SAMPLE_BYTES), m->max_frames);
		hdr = le32_to_cpup((const __le32 *)pkt);
		ffuc_fb_put(chip, (hdr >> 5) & (ffuc_iso_wrap(chip) - 1),
			    frames[i],
			    chip->ucx ? ((hdr >> 21) & 0x7ff) :
					((hdr >> 18) & 0x3fff));
	}
	spin_unlock_irqrestore(&chip->fb_lock, flags);

	snd_pcm_stream_lock_irqsave(subs, flags);
	if (s->running) {
		for (i = 0; i < urb->number_of_packets; i++) {
			const u8 *pkt = (const u8 *)urb->transfer_buffer +
					urb->iso_frame_desc[i].offset;

			if (!frames[i])
				continue;
			ffuc_capture_copy(subs->runtime, s->pos[0],
					  (const __le32 *)(pkt + FFUC_ISO_IN_HEADER),
					  frames[i], nch, 0, 1, frames[i], nch);
			elapsed |= ffuc_stream_advance(chip, s, subs->runtime, 0,
						       frames[i]);
		}
		if (elapsed)
			snd_pcm_period_elapsed_under_stream_lock(subs);
	}
	snd_pcm_stream_unlock_irqrestore(subs, flags);
}

/*
 * ISO OUT packet:
 *   UCX: frames interleaved, each frame = lead word + nch float32 samples.
 *        The first frame's lead word is the ring position the packet plays
 *        at (mod 3072).  Writing all lead words keeps every frame
 *        self-describing and matches the UCX's own format.
 *   UC:  frames interleaved, each frame = lead word + nch float32 samples;
 *        only the first lead word is used by the device.
 */
static void ffuc_iso_playback_fill(struct ffuc_ep *ep, struct urb *urb)
{
	struct ffuc *chip = ep->chip;
	struct ffuc_stream *s = &chip->streams[SNDRV_PCM_STREAM_PLAYBACK];
	struct snd_pcm_substream *subs = s->substream;
	const struct ffuc_mode *m = chip->mode;
	const unsigned int nch = m->words;
	const unsigned int fwords = nch + 1;
	unsigned int frames[FFUC_MAX_ISO_PACKETS], pos[FFUC_MAX_ISO_PACKETS];
	unsigned int phase[FFUC_MAX_ISO_PACKETS];
	unsigned int off = 0;
	unsigned long flags;
	bool elapsed = false;
	int i;

	spin_lock_irqsave(&chip->fb_lock, flags);
	for (i = 0; i < urb->number_of_packets; i++) {
		phase[i] = 0;
		if (!ffuc_fb_get(chip, chip->out_pkts++, &pos[i], &frames[i],
				 &phase[i]))
			frames[i] = 0;
	}
	spin_unlock_irqrestore(&chip->fb_lock, flags);

	snd_pcm_stream_lock_irqsave(subs, flags);
	if (s->running && !s->out_counter_valid) {
		for (i = 0; i < urb->number_of_packets; i++) {
			if (frames[i]) {
				s->out_counter = (u32)pos[i] + chip->out_lead;
				s->out_counter_valid = true;
				break;
			}
		}
	}
	for (i = 0; i < urb->number_of_packets; i++) {
		__le32 *dst = (__le32 *)((u8 *)urb->transfer_buffer + off);
		unsigned int len = frames[i] * fwords * FFUC_SAMPLE_BYTES;
		unsigned int f;

		urb->iso_frame_desc[i].offset = off;
		urb->iso_frame_desc[i].length = len;
		if (s->running && frames[i]) {
			ffuc_playback_copy(subs->runtime, s->pos[0], dst + 1,
					   frames[i], nch, 0, fwords);
			elapsed |= ffuc_stream_advance(chip, s, subs->runtime, 0,
						       frames[i]);
		} else {
			memset(dst, 0, len);
		}
		if (frames[i]) {
			u32 base = s->out_counter;

			if (chip->ucx) {
				for (f = 0; f < frames[i]; f++)
					dst[f * fwords] = cpu_to_le32(
						(base + f) %
						ffuc_iso_wrap(chip));
			} else {
				dst[0] = cpu_to_le32(base);
			}
			s->out_counter += frames[i];
		}
		off += len;
	}
	urb->transfer_buffer_length = off;
	if (elapsed)
		snd_pcm_period_elapsed_under_stream_lock(subs);
	snd_pcm_stream_unlock_irqrestore(subs, flags);
}

static bool ffuc_iso_contiguous(struct ffuc_ep *ep, struct urb *urb)
{
	int expected = ep->next_start;

	ep->next_start = (urb->start_frame + urb->number_of_packets) &
			 FFUC_ISO_FRAME_MASK;
	return expected < 0 ||
	       ((urb->start_frame - expected) & FFUC_ISO_FRAME_MASK) == 0;
}

static void ffuc_stop_running_streams(struct ffuc *chip)
{
	int dir;

	for (dir = 0; dir < 2; dir++) {
		struct snd_pcm_substream *subs = chip->streams[dir].substream;

		if (subs)
			snd_pcm_stop_xrun(subs);
	}
}

static void ffuc_track_playback_urb(struct ffuc *chip, struct urb *urb)
{
	s64 expected, delta;
	ktime_t now = ktime_get();

	if (urb->status)
		atomic_inc(&chip->out_status_errors);
	if (urb->actual_length != urb->transfer_buffer_length)
		atomic_inc(&chip->out_shorts);
	atomic_inc(&chip->out_urbs);
	if (!chip->rate || !READ_ONCE(chip->engine_running))
		return;
	expected = (s64)chip->urb_frames * NSEC_PER_SEC / chip->rate;
	if (chip->last_out_complete) {
		delta = ktime_to_ns(ktime_sub(now, chip->last_out_complete));
		if (delta > expected * 3)
			atomic_inc(&chip->out_gaps);
	}
	chip->last_out_complete = now;
}

static int ffuc_stats_show(struct seq_file *m, void *v)
{
	struct ffuc *chip = m->private;

	seq_printf(m, "out_urbs %d\n", atomic_read(&chip->out_urbs));
	seq_printf(m, "out_gaps %d\n", atomic_read(&chip->out_gaps));
	seq_printf(m, "out_shorts %d\n", atomic_read(&chip->out_shorts));
	seq_printf(m, "out_status_errors %d\n",
		   atomic_read(&chip->out_status_errors));
	seq_printf(m, "urb_errors %d\n", atomic_read(&chip->urb_errors));
	seq_printf(m, "recoveries %u\n", chip->recoveries);
	seq_printf(m, "events_dropped %d\n", atomic_read(&chip->events_dropped));
	seq_printf(m, "urb_frames %u\nnurbs %u\n", chip->urb_frames,
		   chip->nurbs);
	return 0;
}
DEFINE_SHOW_ATTRIBUTE(ffuc_stats);

static void ffuc_urb_complete(struct urb *urb)
{
	struct ffuc_urb *u = urb->context;
	struct ffuc_ep *ep = u->ep;
	struct ffuc *chip = ep->chip;
	int ret;

	if (ep->dir == SNDRV_PCM_STREAM_PLAYBACK &&
	    urb->status != -ENOENT && urb->status != -ECONNRESET &&
	    urb->status != -ESHUTDOWN && urb->status != -ENODEV)
		ffuc_track_playback_urb(chip, urb);

	switch (urb->status) {
	case 0:
		atomic_set(&chip->urb_errors, 0);
		break;
	case -ENOENT:
	case -ECONNRESET:
	case -ESHUTDOWN:
	case -ENODEV:
		return;
	case -EXDEV:
		if (chip->iso)
			break;
		fallthrough;
	default:
		dev_dbg_ratelimited(&chip->dev->dev, "%s URB status %d\n",
				    ep->dir ? "IN" : "OUT", urb->status);
		if (atomic_inc_return(&chip->urb_errors) > FFUC_MAX_URB_ERRORS) {
			dev_err(&chip->dev->dev, "too many URB errors, stopping\n");
			goto broken;
		}
		if (!chip->iso) {
			if (ep->dir == SNDRV_PCM_STREAM_PLAYBACK)
				memset(urb->transfer_buffer, 0,
				       urb->transfer_buffer_length);
			goto resubmit;
		}
	}

	if (!READ_ONCE(chip->engine_running))
		return;

	if (ep->dir == SNDRV_PCM_STREAM_CAPTURE) {
		if (chip->iso)
			ffuc_iso_capture_urb(ep, urb);
		else
			ffuc_capture_urb(ep, urb);
	} else {
		if (chip->iso)
			ffuc_iso_playback_fill(ep, urb);
		else
			ffuc_playback_urb(ep, urb);
	}

resubmit:
	if (!READ_ONCE(chip->engine_running))
		return;
	ret = usb_submit_urb(urb, GFP_ATOMIC);
	if (ret == 0) {
		if (!READ_ONCE(chip->iso_frames_ok) ||
		    ffuc_iso_contiguous(ep, urb))
			return;
		dev_dbg_ratelimited(&chip->dev->dev, "%s URB queue ran dry\n",
				    ep->dir ? "IN" : "OUT");
		goto broken;
	}
	if (ret == -EPERM || ret == -ENOENT || ret == -ENODEV ||
	    ret == -ESHUTDOWN || !READ_ONCE(chip->engine_running))
		return;
	dev_err_ratelimited(&chip->dev->dev, "URB resubmit failed: %d\n", ret);
broken:
	if (!READ_ONCE(chip->engine_broken)) {
		WRITE_ONCE(chip->engine_broken, true);
		queue_work(system_unbound_wq, &chip->recover_work);
	}
}

static void ffuc_status_complete(struct urb *urb);

static int ffuc_status_submit(struct ffuc *chip)
{
	int ret;

	if (!chip->status_intf || chip->status_active ||
	    READ_ONCE(chip->disconnected))
		return 0;
	usb_fill_int_urb(chip->status_urb, chip->dev,
			 usb_rcvintpipe(chip->dev, chip->desc->status_ep),
			 chip->status_buf, FFUC_STATUS_LEN,
			 ffuc_status_complete, chip, 1);
	WRITE_ONCE(chip->status_armed_at,
		   jiffies + msecs_to_jiffies(FFUC_STATUS_GRACE_MS));
	ret = usb_submit_urb(chip->status_urb, GFP_ATOMIC);
	if (!ret)
		chip->status_active = true;
	return ret;
}

static void ffuc_status_complete(struct urb *urb)
{
	struct ffuc *chip = urb->context;
	bool latched;
	int ret;

	chip->status_active = false;

	switch (urb->status) {
	case 0:
		break;
	case -ENOENT:
	case -ECONNRESET:
	case -ESHUTDOWN:
	case -ENODEV:
		return;
	default:
		goto resubmit;
	}

	if (urb->actual_length >= FFUC_STATUS_LEN) {
		memcpy(chip->last_status, chip->status_buf, FFUC_STATUS_LEN);
		ffuc_event_put(chip, FFUC_EV_STATUS, chip->status_buf,
			       min_t(unsigned int, urb->actual_length,
				     FFUC_EVENT_MAX), 0);
	}

	latched = (chip->desc->quirks & FFUC_QUIRK_STATUS_LATCH) &&
		  chip->last_status[3] & FFUC_STATUS_OUT_LATCHED;
	if (latched != READ_ONCE(chip->out_latched)) {
		WRITE_ONCE(chip->out_latched, latched);
		ffuc_event_put(chip, FFUC_EV_LATCH, NULL, 0,
			       latched ? 1 : 0);
	}

	if (latched &&
	    time_after(jiffies, READ_ONCE(chip->status_armed_at)) &&
	    READ_ONCE(chip->engine_running) &&
	    READ_ONCE(chip->streams[SNDRV_PCM_STREAM_PLAYBACK].running) &&
	    !READ_ONCE(chip->engine_broken)) {
		WRITE_ONCE(chip->engine_broken, true);
		queue_work(system_unbound_wq, &chip->recover_work);
		return;
	}

resubmit:
	/* The UC keeps the ticker open all the time (front-panel events);
	 * the UCX IF5 endpoint only reports stream-period ticks. */
	if (chip->ucx && !READ_ONCE(chip->engine_running))
		return;
	ret = ffuc_status_submit(chip);
	if (ret < 0 && ret != -EPERM)
		dev_dbg_ratelimited(&chip->dev->dev,
				    "status URB resubmit failed: %d\n", ret);
}

static void ffuc_kill_urbs(struct ffuc *chip, bool kill_status)
{
	int dir, e, i;

	for (dir = 0; dir < 2; dir++)
		for (e = 0; e < FFUC_MAX_EPS; e++)
			for (i = 0; i < chip->nurbs; i++)
				usb_kill_urb(chip->eps[dir][e].urbs[i].urb);
	if (kill_status) {
		usb_kill_urb(chip->status_urb);
		chip->status_active = false;
	}
}

static void ffuc_engine_stop(struct ffuc *chip)
{
	if (!chip->engine_running)
		return;
	dev_info(&chip->dev->dev,
		 "engine stop: rate=%u session=%d\n",
		 chip->rate, chip->session_reopen);
	/* Windows sends the session-close word while packets are still
	 * flowing (A2 t=13.03, stream dies at 13.3), and does not send a
	 * separate mute-all.  The UC close word is 0x13/0x000f; a later
	 * 0x13/0xffff would mute outputs the session-open 0x14/0x000f never
	 * unmutes. */
	if (!READ_ONCE(chip->disconnected)) {
		chip->desc->ops->stream_stop(chip, ffuc_mode_channels(chip,
								     chip->rate));
		if (chip->model != FFUC_MODEL_UC)
			chip->desc->ops->hw_mute(chip, true);
	}
	WRITE_ONCE(chip->engine_running, false);
	ffuc_kill_urbs(chip, chip->ucx || chip->iso);
	dev_dbg(&chip->dev->dev, "engine stopped\n");
}

static void ffuc_int_setup_urbs(struct ffuc *chip, unsigned int bytes)
{
	const struct ffuc_mode *m = chip->mode;
	unsigned int in_bytes = bytes;
	int dir, e, i;

	/* The Windows IF5 receive path always uses full 1024-frame URBs. */
	if (chip->ucx && !chip->iso)
		in_bytes = chip->base_urb_frames * ffuc_max_words(chip) *
			   FFUC_SAMPLE_BYTES;

	for (dir = 0; dir < 2; dir++) {
		for (e = 0; e < m->neps; e++) {
			struct ffuc_ep *ep = &chip->eps[dir][e];
			unsigned int pipe;
			unsigned int len;

			if (dir == SNDRV_PCM_STREAM_PLAYBACK)
				pipe = usb_sndintpipe(chip->dev, 1 + 2 * e);
			else
				pipe = usb_rcvintpipe(chip->dev, 2 + 2 * e);
			len = dir == SNDRV_PCM_STREAM_PLAYBACK ? bytes :
							       in_bytes;

			for (i = 0; i < chip->nurbs; i++) {
				struct ffuc_urb *u = &ep->urbs[i];

				usb_fill_int_urb(u->urb, chip->dev, pipe, u->buf,
						 len, ffuc_urb_complete, u, 1);
				u->urb->transfer_dma = u->dma;
				u->urb->transfer_flags |= URB_NO_TRANSFER_DMA_MAP;
				if (dir == SNDRV_PCM_STREAM_PLAYBACK)
					memset(u->buf, 0, len);
			}
		}
	}
}

static void ffuc_iso_setup_urbs(struct ffuc *chip)
{
	const struct ffuc_mode *m = chip->mode;
	int dir, i, p;

	chip->in_pkts = chip->out_pkts = 0;
	chip->streams[SNDRV_PCM_STREAM_PLAYBACK].out_counter_valid = false;
	chip->iso_frames_ok = false;
	chip->in_end = 0;
	chip->fb_acc = 0;
	chip->fb_delay = (chip->nurbs + 1) * chip->iso_packets;
	/* OUT data is placed this many frames after the IN position it
	 * mirrors.  The UCX needs OUT_LEAD (60) frames of margin at the
	 * unit; the UC path used the vendor's 2 * max_frames - 2 margin. */
	chip->out_lead = (DIV_ROUND_CLOSEST(chip->fb_delay * chip->rate,
					    8000) +
			  (chip->ucx ? 60 : 2 * m->max_frames - 2)) &
			 (ffuc_iso_wrap(chip) - 1);

	for (dir = 0; dir < 2; dir++) {
		struct ffuc_ep *ep = &chip->eps[dir][0];

		ep->next_start = -1;
		for (i = 0; i < chip->nurbs; i++) {
			struct urb *urb = ep->urbs[i].urb;

			urb->dev = chip->dev;
			urb->pipe = dir == SNDRV_PCM_STREAM_PLAYBACK ?
				    usb_sndisocpipe(chip->dev, chip->out_ep) :
				    usb_rcvisocpipe(chip->dev, chip->in_ep);
			urb->interval = 1;
			urb->transfer_flags = URB_ISO_ASAP |
					      URB_NO_TRANSFER_DMA_MAP;
			urb->transfer_buffer = ep->urbs[i].buf;
			urb->transfer_dma = ep->urbs[i].dma;
			urb->number_of_packets = chip->iso_packets;
			urb->complete = ffuc_urb_complete;
			urb->context = &ep->urbs[i];
			if (dir == SNDRV_PCM_STREAM_PLAYBACK) {
				ffuc_iso_playback_fill(ep, urb);
				continue;
			}
			urb->transfer_buffer_length = chip->iso_packets *
						      m->in_mps;
			for (p = 0; p < chip->iso_packets; p++) {
				urb->iso_frame_desc[p].offset = p * m->in_mps;
				urb->iso_frame_desc[p].length = m->in_mps;
			}
		}
	}
}

static void ffuc_iso_align(struct ffuc *chip)
{
	struct ffuc_ep *ein = &chip->eps[SNDRV_PCM_STREAM_CAPTURE][0];
	struct ffuc_ep *eout = &chip->eps[SNDRV_PCM_STREAM_PLAYBACK][0];
	int in = ein->urbs[0].urb->start_frame;
	int out = eout->urbs[0].urb->start_frame;
	int diff = out - in;
	bool ok = true;
	unsigned long flags;
	int i;

	for (i = 0; i < chip->nurbs; i++) {
		ok &= ffuc_iso_contiguous(ein, ein->urbs[i].urb);
		ok &= ffuc_iso_contiguous(eout, eout->urbs[i].urb);
	}
	if (!ok || abs(diff) > 64)
		diff = 0;
	spin_lock_irqsave(&chip->fb_lock, flags);
	chip->out_lead = (chip->out_lead + ffuc_iso_wrap(chip) +
			  diff * (int)chip->rate / 8000) %
			 ffuc_iso_wrap(chip);
	spin_unlock_irqrestore(&chip->fb_lock, flags);
	WRITE_ONCE(chip->iso_frames_ok, ok);
	dev_dbg(&chip->dev->dev, "start frames IN %d OUT %d%s, OUT lead %u\n",
		in, out, ok ? "" : " (not contiguous, unchecked)", chip->out_lead);
}

static int ffuc_engine_start(struct ffuc *chip)
{
	const struct ffuc_mode *m = chip->mode;
	unsigned int bytes;
	int e, i, ret;

	if (chip->engine_running)
		return 0;
	if (READ_ONCE(chip->disconnected))
		return -ENODEV;

	chip->urb_frames = ffuc_urb_frames(chip, chip->rate);
	if (chip->iso)
		bytes = chip->iso_packets * max(m->in_mps, m->out_mps);
	else
		bytes = chip->urb_frames * m->words * FFUC_SAMPLE_BYTES;
	if (WARN_ON(bytes > chip->urb_bytes_max))
		return -EINVAL;

	/* alt 0 -> alt N reselect clears the UCX IF5 output latch. */
	if (chip->desc->quirks & FFUC_QUIRK_ALT_RESELECT) {
		ret = usb_set_interface(chip->dev, chip->audio_iface, 0);
		if (ret < 0)
			return ret;
		chip->cur_alt = 0;
	}
	/* Windows never issues SET_INTERFACE for the UC, including across a
	 * 48 <-> 44.1 live rate change (both use alt 1).  Re-selecting the
	 * same alt on the rate-change restart resets the streaming endpoints
	 * after the session words and leaves both directions silent.  Only
	 * program a genuinely different alt (2x/4x, or first start). */
	{
		bool skip_alt = chip->model == FFUC_MODEL_UC &&
				chip->uc_session &&
				chip->cur_alt == m->alt;

		dev_info(&chip->dev->dev,
			 "engine start: rate=%u alt=%u cur_alt=%u session=%d skip_alt=%d\n",
			 chip->rate, m->alt, chip->cur_alt,
			 chip->session_reopen, skip_alt);
		if (!skip_alt) {
			ret = usb_set_interface(chip->dev, chip->audio_iface,
						m->alt);
			if (ret < 0)
				return ret;
			chip->cur_alt = m->alt;
		}
	}
	/* On a same-rate reopen Windows sends only the session trio; it does
	 * not rewrite the rate register.  Re-writing it right after the close
	 * (with no settle gap) wedges the UC.  Only write the rate when the
	 * rate actually changed, and then leave the same 200 ms gap Windows
	 * shows between the session close and the rate write (A2: 220 ms). */
	if (chip->model != FFUC_MODEL_UC || chip->session_reopen) {
		if (chip->model == FFUC_MODEL_UC)
			msleep(200);
		ret = chip->desc->ops->set_rate(chip, chip->rate);
		if (ret < 0)
			return ret;
	}

	/* Windows sends the session-open trio before the stream starts, not
	 * after the URBs are submitted.  On the UC the trio is what opens the
	 * device's audio path at all; sending it after the endpoints are
	 * already running leaves the stream alive with silence in both
	 * directions.  The later stream_start call is a no-op for the UC once
	 * session_reopen has been cleared. */
	if (chip->model == FFUC_MODEL_UC && chip->uc_session) {
		/* Windows leaves seconds between a stream close and the next
		 * open (A2b: 6.64 -> 10.87).  A back-to-back reopen after the
		 * 0x13 close leaves the UC silent even when the rate did not
		 * change.  Give the close time to take effect. */
		msleep(200);
		ret = chip->desc->ops->stream_start(
			chip, ffuc_mode_channels(chip, chip->rate));
		if (ret < 0)
			return ret;
	}

	if (chip->iso)
		ffuc_iso_setup_urbs(chip);
	else
		ffuc_int_setup_urbs(chip, bytes);

	atomic_set(&chip->urb_errors, 0);
	chip->last_out_complete = 0;
	WRITE_ONCE(chip->engine_broken, false);
	WRITE_ONCE(chip->engine_running, true);
	for (i = 0; i < chip->nurbs; i++) {
		for (e = 0; e < m->neps; e++) {
			ret = usb_submit_urb(
				chip->eps[SNDRV_PCM_STREAM_CAPTURE][e].urbs[i].urb,
				GFP_KERNEL);
			if (ret < 0)
				goto error;
			ret = usb_submit_urb(
				chip->eps[SNDRV_PCM_STREAM_PLAYBACK][e].urbs[i].urb,
				GFP_KERNEL);
			if (ret < 0)
				goto error;
		}
	}

	/* The UC already sent its session-open trio before the URBs.  The
	 * UCX still needs its stream_start after the URBs are submitted. */
	if (chip->model != FFUC_MODEL_UC) {
		ret = chip->desc->ops->stream_start(
			chip, ffuc_mode_channels(chip, chip->rate));
		if (ret < 0)
			goto error;
	}

	if (chip->iso)
		ffuc_iso_align(chip);

	WRITE_ONCE(chip->status_armed_at,
		   jiffies + msecs_to_jiffies(FFUC_STATUS_GRACE_MS));
	ret = ffuc_status_submit(chip);
	if (ret < 0)
		dev_warn(&chip->dev->dev,
			 "cannot watch the status endpoint: %d\n", ret);

	ret = chip->desc->ops->hw_mute(chip, false);
	if (ret < 0)
		goto error;

	dev_dbg(&chip->dev->dev,
		"engine started: %u Hz, alt %u, %u frames x %u URBs\n",
		chip->rate, m->alt, chip->urb_frames, chip->nurbs);
	return 0;

error:
	dev_err(&chip->dev->dev, "failed to start streaming: %d\n", ret);
	WRITE_ONCE(chip->engine_running, false);
	ffuc_kill_urbs(chip, chip->ucx || chip->iso);
	return ret;
}

static void ffuc_recover_work(struct work_struct *work)
{
	struct ffuc *chip = container_of(work, struct ffuc, recover_work);
	int ret;

	mutex_lock(&chip->mutex);
	if (READ_ONCE(chip->disconnected) || !chip->engine_running ||
	    !READ_ONCE(chip->engine_broken))
		goto unlock;

	chip->recoveries++;
	dev_warn_ratelimited(&chip->dev->dev,
			     "stream interrupted (output underrun or USB error), restarting (%u)\n",
			     chip->recoveries);
	ffuc_stop_running_streams(chip);
	ffuc_engine_stop(chip);
	if (chip->prepared_mask) {
		ret = ffuc_engine_start(chip);
		if (ret < 0)
			dev_err(&chip->dev->dev, "restart failed: %d\n", ret);
	}
unlock:
	mutex_unlock(&chip->mutex);
}

/* -- PCM ------------------------------------------------------------- */

static const struct snd_pcm_hardware ffuc_pcm_hw = {
	.info = SNDRV_PCM_INFO_MMAP |
		SNDRV_PCM_INFO_MMAP_VALID |
		SNDRV_PCM_INFO_INTERLEAVED |
		SNDRV_PCM_INFO_BLOCK_TRANSFER,
	.formats = SNDRV_PCM_FMTBIT_FLOAT_LE | SNDRV_PCM_FMTBIT_S32_LE,
	.rates = SNDRV_PCM_RATE_KNOT,
	.rate_min = 32000,
	.rate_max = FFUC_MAX_RATE,
	.channels_min = 1,
	.channels_max = FFUC_MAX_CHANNELS,
	.buffer_bytes_max = 4 * 1024 * 1024,
	.period_bytes_min = 32,
	.period_bytes_max = 1024 * 1024,
	.periods_min = 2,
	.periods_max = 1024,
};

static int ffuc_rule_channels(struct snd_pcm_hw_params *params,
			      struct snd_pcm_hw_rule *rule)
{
	struct ffuc *chip = rule->private;
	struct snd_interval *r = hw_param_interval(params, SNDRV_PCM_HW_PARAM_RATE);
	struct snd_interval t = {
		.min = 1,
		.max = ffuc_max_channels(chip, r->min),
		.integer = 1,
	};

	return snd_interval_refine(hw_param_interval(params,
				   SNDRV_PCM_HW_PARAM_CHANNELS), &t);
}

static int ffuc_rule_rate(struct snd_pcm_hw_params *params,
			  struct snd_pcm_hw_rule *rule)
{
	struct snd_pcm_substream *subs = rule->private;
	struct ffuc *chip = snd_pcm_substream_chip(subs);
	unsigned int other = 1 << !subs->stream;
	struct snd_interval t = {
		.min = 32000,
		.max = min(chip->desc->max_rate, (unsigned int)FFUC_MAX_RATE),
		.integer = 1,
	};
	unsigned int locked = 0;

	if (READ_ONCE(chip->hw_params_mask) & other)
		locked = READ_ONCE(chip->rate);
	else if (ffuc_rate_supported(chip, READ_ONCE(fixed_rate)))
		locked = READ_ONCE(fixed_rate);
	if (locked) {
		t.min = max(t.min, locked);
		t.max = min(t.max, locked);
	}
	return snd_interval_refine(hw_param_interval(params,
				   SNDRV_PCM_HW_PARAM_RATE), &t);
}

static int ffuc_rule_period(struct snd_pcm_hw_params *params,
			    struct snd_pcm_hw_rule *rule)
{
	struct ffuc *chip = rule->private;
	struct snd_interval *r = hw_param_interval(params, SNDRV_PCM_HW_PARAM_RATE);
	/*
	 * The ALSA period and the USB URB are separate buffers.  The period
	 * floor is one URB (48 frames at 1x, scaled at 2x/4x); the graph
	 * quantum follows at 48 frames and above.
	 */
	struct snd_interval t = {
		.min = ffuc_urb_frames(chip, r->min),
		.max = UINT_MAX,
		.integer = 1,
	};

	return snd_interval_refine(hw_param_interval(params,
				   SNDRV_PCM_HW_PARAM_PERIOD_SIZE), &t);
}

static int ffuc_rule_buffer(struct snd_pcm_hw_params *params,
			    struct snd_pcm_hw_rule *rule)
{
	struct ffuc *chip = rule->private;
	struct snd_interval *r = hw_param_interval(params, SNDRV_PCM_HW_PARAM_RATE);
	/*
	 * The ALSA ring must cover the USB URBs already in flight plus two
	 * more URBs of margin.  Two periods are exactly the queue at the
	 * 48-frame floor (no margin) and leave only 32 frames at 64, so the
	 * transport underruns there.  The rule raises 48 samples to four
	 * periods, 64 to three, and keeps 96 and 128 at two.
	 */
	struct snd_interval t = {
		.min = (chip->nurbs + 2) * ffuc_urb_frames(chip, r->min),
		.max = UINT_MAX,
		.integer = 1,
	};

	return snd_interval_refine(hw_param_interval(params,
				   SNDRV_PCM_HW_PARAM_BUFFER_SIZE), &t);
}

static int ffuc_pcm_open(struct snd_pcm_substream *subs)
{
	struct ffuc *chip = snd_pcm_substream_chip(subs);
	struct snd_pcm_runtime *rt = subs->runtime;
	int ret;

	if (READ_ONCE(chip->disconnected))
		return -ENODEV;

	rt->hw = ffuc_pcm_hw;
	ret = snd_pcm_hw_constraint_list(rt, 0, SNDRV_PCM_HW_PARAM_RATE,
					 &ffuc_rate_list);
	if (ret < 0)
		return ret;
	ret = snd_pcm_hw_rule_add(rt, 0, SNDRV_PCM_HW_PARAM_CHANNELS,
				  ffuc_rule_channels, chip,
				  SNDRV_PCM_HW_PARAM_RATE, -1);
	if (ret < 0)
		return ret;
	ret = snd_pcm_hw_rule_add(rt, 0, SNDRV_PCM_HW_PARAM_RATE,
				  ffuc_rule_rate, subs,
				  SNDRV_PCM_HW_PARAM_CHANNELS, -1);
	if (ret < 0)
		return ret;
	ret = snd_pcm_hw_rule_add(rt, 0, SNDRV_PCM_HW_PARAM_PERIOD_SIZE,
				  ffuc_rule_period, chip,
				  SNDRV_PCM_HW_PARAM_RATE, -1);
	if (ret < 0)
		return ret;
	ret = snd_pcm_hw_rule_add(rt, 0, SNDRV_PCM_HW_PARAM_BUFFER_SIZE,
				  ffuc_rule_buffer, chip,
				  SNDRV_PCM_HW_PARAM_RATE, -1);
	if (ret < 0)
		return ret;
	return snd_pcm_hw_constraint_msbits(rt, 0, 32, 24);
}

static int ffuc_pcm_close(struct snd_pcm_substream *subs)
{
	return 0;
}

static int ffuc_pcm_hw_params(struct snd_pcm_substream *subs,
			      struct snd_pcm_hw_params *params)
{
	struct ffuc *chip = snd_pcm_substream_chip(subs);
	unsigned int rate = params_rate(params);
	unsigned int self = 1 << subs->stream;
	int ret = 0;

	if (params_channels(params) > ffuc_max_channels(chip, rate))
		return -EINVAL;

	mutex_lock(&chip->mutex);
	if (rate != chip->rate || !chip->mode) {
		dev_info(&chip->dev->dev,
			 "hw_params: stream=%s rate=%u old=%u engine=%d\n",
			 subs->stream == SNDRV_PCM_STREAM_PLAYBACK ?
			 "playback" : "capture",
			 rate, chip->rate, chip->engine_running);
		if ((chip->hw_params_mask | chip->prepared_mask) & ~self) {
			ret = -EBUSY;
			goto unlock;
		}
		/* Stop before arming the session flags.  The first rate-change
		 * stop must not send the session-close word (A2 sends nothing
		 * between the last tone packet and the rate write); from then
		 * on every UC stop/open gets the session words. */
		ffuc_engine_stop(chip);
		if (chip->model == FFUC_MODEL_UC) {
			chip->session_reopen = true;
			chip->uc_session = true;
		}
		chip->prepared_mask &= ~self;
		chip->rate = rate;
		chip->mode = &chip->modes[ffuc_mode_index(rate)];
	}
	chip->hw_params_mask |= self;
unlock:
	mutex_unlock(&chip->mutex);
	return ret;
}

static int ffuc_pcm_hw_free(struct snd_pcm_substream *subs)
{
	struct ffuc *chip = snd_pcm_substream_chip(subs);
	unsigned int self = 1 << subs->stream;

	mutex_lock(&chip->mutex);
	chip->hw_params_mask &= ~self;
	chip->prepared_mask &= ~self;
	if (!chip->prepared_mask)
		ffuc_engine_stop(chip);
	mutex_unlock(&chip->mutex);
	return 0;
}

static int ffuc_pcm_prepare(struct snd_pcm_substream *subs)
{
	struct ffuc *chip = snd_pcm_substream_chip(subs);
	struct ffuc_stream *s = &chip->streams[subs->stream];
	int ret;

	mutex_lock(&chip->mutex);
	if (READ_ONCE(chip->engine_broken))
		ffuc_engine_stop(chip);
	ret = ffuc_engine_start(chip);
	if (ret == 0)
		chip->prepared_mask |= 1 << subs->stream;
	mutex_unlock(&chip->mutex);
	if (ret < 0)
		return ret;

	snd_pcm_stream_lock_irq(subs);
	s->running = false;
	memset(s->pos, 0, sizeof(s->pos));
	s->hw_ptr = 0;
	s->period = 0;
	if (subs->stream == SNDRV_PCM_STREAM_PLAYBACK)
		subs->runtime->delay = chip->nurbs * chip->urb_frames;
	snd_pcm_stream_unlock_irq(subs);
	return 0;
}

static int ffuc_pcm_trigger(struct snd_pcm_substream *subs, int cmd)
{
	struct ffuc *chip = snd_pcm_substream_chip(subs);
	struct ffuc_stream *s = &chip->streams[subs->stream];

	switch (cmd) {
	case SNDRV_PCM_TRIGGER_START:
		if (!READ_ONCE(chip->engine_running))
			return -EIO;
		memset(s->pos, 0, sizeof(s->pos));
		s->hw_ptr = 0;
		s->period = 0;
		s->running = true;
		return 0;
	case SNDRV_PCM_TRIGGER_STOP:
	case SNDRV_PCM_TRIGGER_SUSPEND:
		s->running = false;
		return 0;
	default:
		return -EINVAL;
	}
}

static snd_pcm_uframes_t ffuc_pcm_pointer(struct snd_pcm_substream *subs)
{
	struct ffuc *chip = snd_pcm_substream_chip(subs);

	return chip->streams[subs->stream].hw_ptr %
	       subs->runtime->buffer_size;
}

static const struct snd_pcm_ops ffuc_pcm_ops = {
	.open = ffuc_pcm_open,
	.close = ffuc_pcm_close,
	.hw_params = ffuc_pcm_hw_params,
	.hw_free = ffuc_pcm_hw_free,
	.prepare = ffuc_pcm_prepare,
	.trigger = ffuc_pcm_trigger,
	.pointer = ffuc_pcm_pointer,
};

static int ffuc_create_pcm(struct ffuc *chip)
{
	struct snd_pcm *pcm;
	int ret;

	ret = snd_pcm_new(chip->card, chip->desc->name, 0, 1, 1, &pcm);
	if (ret < 0)
		return ret;
	pcm->private_data = chip;
	strscpy(pcm->name, chip->desc->name, sizeof(pcm->name));
	snd_pcm_set_ops(pcm, SNDRV_PCM_STREAM_PLAYBACK, &ffuc_pcm_ops);
	snd_pcm_set_ops(pcm, SNDRV_PCM_STREAM_CAPTURE, &ffuc_pcm_ops);
	snd_pcm_set_managed_buffer_all(pcm, SNDRV_DMA_TYPE_VMALLOC, NULL, 0, 0);

	chip->streams[SNDRV_PCM_STREAM_PLAYBACK].substream =
		pcm->streams[SNDRV_PCM_STREAM_PLAYBACK].substream;
	chip->streams[SNDRV_PCM_STREAM_CAPTURE].substream =
		pcm->streams[SNDRV_PCM_STREAM_CAPTURE].substream;
	chip->pcm = pcm;
	return 0;
}

/* -- USB driver ------------------------------------------------------ */

static int ffuc_alloc_urbs(struct ffuc *chip)
{
	int dir, e, i;

	chip->status_urb = usb_alloc_urb(0, GFP_KERNEL);
	chip->status_buf = kmalloc(FFUC_STATUS_LEN, GFP_KERNEL);
	if (!chip->status_urb || !chip->status_buf)
		return -ENOMEM;

	for (dir = 0; dir < 2; dir++) {
		for (e = 0; e < FFUC_MAX_EPS; e++) {
			struct ffuc_ep *ep = &chip->eps[dir][e];

			ep->chip = chip;
			ep->dir = dir;
			ep->index = e;
			for (i = 0; i < chip->nurbs; i++) {
				struct ffuc_urb *u = &ep->urbs[i];

				u->ep = ep;
				u->urb = usb_alloc_urb(chip->iso ?
						       FFUC_MAX_ISO_PACKETS : 0,
						       GFP_KERNEL);
				if (!u->urb)
					return -ENOMEM;
				u->buf = usb_alloc_coherent(chip->dev,
							    chip->urb_bytes_max,
							    GFP_KERNEL, &u->dma);
				if (!u->buf)
					return -ENOMEM;
			}
		}
	}
	return 0;
}

static void ffuc_free_urbs(struct ffuc *chip)
{
	int dir, e, i;

	if (chip->status_urb) {
		usb_kill_urb(chip->status_urb);
		usb_free_urb(chip->status_urb);
		chip->status_urb = NULL;
	}
	kfree(chip->status_buf);
	chip->status_buf = NULL;

	for (dir = 0; dir < 2; dir++) {
		for (e = 0; e < FFUC_MAX_EPS; e++) {
			for (i = 0; i < FFUC_MAX_URBS; i++) {
				struct ffuc_urb *u = &chip->eps[dir][e].urbs[i];

				if (u->urb) {
					usb_kill_urb(u->urb);
					usb_free_urb(u->urb);
					u->urb = NULL;
				}
				if (u->buf) {
					usb_free_coherent(chip->dev,
							  chip->urb_bytes_max,
							  u->buf, u->dma);
					u->buf = NULL;
				}
			}
		}
	}
}

static void ffuc_card_free(struct snd_card *card)
{
	struct ffuc *chip = card->private_data;

	cancel_work_sync(&chip->recover_work);
	ffuc_hwdep_cleanup(chip);
	ffuc_midi_cleanup(chip);
	if (chip->status_intf) {
		usb_driver_release_interface(&ffuc_driver, chip->status_intf);
		chip->status_intf = NULL;
	}
	ffuc_free_urbs(chip);
	usb_put_dev(chip->dev);
	mutex_lock(&devices_mutex);
	clear_bit(chip->dev_idx, &devices_used);
	mutex_unlock(&devices_mutex);
}

static int ffuc_hw_setup(struct ffuc *chip)
{
	int ret;

	ret = usb_set_interface(chip->dev, chip->audio_iface,
				chip->mode->alt);
	if (ret < 0)
		return ret;
	chip->cur_alt = chip->mode->alt;
	ret = chip->desc->ops->init(chip);
	if (ret < 0)
		return ret;
	ret = chip->desc->ops->set_rate(chip, chip->rate);
	if (ret < 0)
		return ret;
	return chip->desc->ops->hw_mute(chip, true);
}

static void ffuc_read_identity(struct ffuc *chip)
{
	u32 rev = 0;

	if (ffuc_vendor_recv(chip, FFUC_REQ_REVISION, 0, 0, &rev,
			     sizeof(rev)) == 0)
		chip->firmware = (le32_to_cpu(rev) >> 16) & 0xff;
	if (chip->dev->serial)
		strscpy(chip->serial, chip->dev->serial, sizeof(chip->serial));
}

static int ffuc_probe(struct usb_interface *intf,
		      const struct usb_device_id *usb_id)
{
	struct usb_device *dev = interface_to_usbdev(intf);
	const struct ffuc_device_desc *desc =
		(const struct ffuc_device_desc *)usb_id->driver_info;
	struct snd_card *card;
	struct ffuc *chip;
	unsigned int iface_no = intf->cur_altsetting->desc.bInterfaceNumber;
	unsigned int max_words;
	int dev_idx, ret;

	if (!desc)
		return -ENODEV;
	if (iface_no != desc->iface)
		return -ENODEV;
	/* Never bind a class-compliant (UAC2) interface.  In CC mode the
	 * device must be left to snd-usb-audio; this driver is only for the
	 * vendor-specific RME personality. */
	if (intf->cur_altsetting->desc.bInterfaceClass !=
	    USB_CLASS_VENDOR_SPEC)
		return -ENODEV;

	/* One card per physical UCX: bind either IF0 or IF5, never both. */
	if (desc->model == FFUC_MODEL_UCX &&
	    ((ucx_if5 && desc->transport != FFUC_XPORT_INTERRUPT) ||
	     (!ucx_if5 && desc->transport != FFUC_XPORT_ISO)))
		return -ENODEV;

	mutex_lock(&devices_mutex);
	for (dev_idx = 0; dev_idx < SNDRV_CARDS; dev_idx++)
		if (enable[dev_idx] && !test_bit(dev_idx, &devices_used))
			break;
	if (dev_idx >= SNDRV_CARDS) {
		mutex_unlock(&devices_mutex);
		return -ENODEV;
	}
	set_bit(dev_idx, &devices_used);
	mutex_unlock(&devices_mutex);

	ret = snd_card_new(&intf->dev, index[dev_idx], id[dev_idx], THIS_MODULE,
			   sizeof(*chip), &card);
	if (ret < 0)
		goto release_idx;

	chip = card->private_data;
	chip->card = card;
	chip->dev = usb_get_dev(dev);
	chip->intf = intf;
	chip->desc = desc;
	chip->model = desc->model;
	chip->iso = desc->transport == FFUC_XPORT_ISO;
	chip->ucx = desc->model == FFUC_MODEL_UCX;
	chip->dev_idx = dev_idx;
	chip->audio_iface = desc->audio_iface;
	chip->out_ep = desc->out_ep;
	chip->in_ep = desc->in_ep;
	chip->modes = desc->modes;
	chip->debugfs = debugfs_create_dir(dev_name(&intf->dev), NULL);
	if (!IS_ERR_OR_NULL(chip->debugfs))
		debugfs_create_file("stats", 0444, chip->debugfs, chip,
				    &ffuc_stats_fops);
	mutex_init(&chip->mutex);
	mutex_init(&chip->ctl_lock);
	spin_lock_init(&chip->fb_lock);
	INIT_WORK(&chip->recover_work, ffuc_recover_work);
	init_waitqueue_head(&chip->event_wait);
	spin_lock_init(&chip->event_lock);
	atomic_set(&chip->event_seq, 0);
	atomic_set(&chip->events_dropped, 0);
	atomic_set(&chip->meter_urbs, 0);
	atomic_set(&chip->state_urbs, 0);
	atomic_set(&chip->hwdep_users, 0);
	if (kfifo_alloc(&chip->event_fifo, FFUC_EVENT_FIFO_SIZE, GFP_KERNEL)) {
		ret = -ENOMEM;
		goto free_card;
	}
	card->private_free = ffuc_card_free;

	chip->nurbs = clamp(nurbs, 2u, (unsigned int)FFUC_MAX_URBS);
	chip->base_urb_frames = clamp(round_up(
		(chip->ucx && !chip->iso) ? ucx_if5_frames : urb_frames, 8),
		8u, (chip->ucx && !chip->iso) ? 1024u : 256u);
	if (chip->nurbs * chip->base_urb_frames < FFUC_MIN_QUEUE_FRAMES) {
		chip->nurbs = DIV_ROUND_UP(FFUC_MIN_QUEUE_FRAMES,
					   chip->base_urb_frames);
		dev_info(&intf->dev, "using %u URBs to keep %u frames queued\n",
			 chip->nurbs, FFUC_MIN_QUEUE_FRAMES);
	}
	chip->iso_packets = clamp(DIV_ROUND_UP(chip->base_urb_frames, 6), 1u,
				  (unsigned int)FFUC_MAX_ISO_PACKETS);
	max_words = ffuc_max_words(chip);
	if (chip->iso) {
		chip->urb_bytes_max = chip->iso_packets *
				      max(chip->modes[0].in_mps,
					  chip->modes[chip->desc->n_modes - 1].out_mps);
	} else {
		unsigned int i, span = 0;

		/* The largest interrupt URB of any rate band: at 1x the base
		 * frame count times 18 words, at 2x twice as many frames times
		 * 14 words, and so on. */
		for (i = 0; i < chip->desc->n_modes; i++) {
			unsigned int frames = (chip->ucx) ?
				chip->base_urb_frames :
				chip->base_urb_frames << i;

			span = max(span, frames * chip->modes[i].words);
		}
		chip->urb_bytes_max = span * FFUC_SAMPLE_BYTES;
	}
	/* The Windows IF5 driver reads 73,728-byte IN URBs. */
	if ((chip->desc->quirks & FFUC_QUIRK_IF5_PLAYBACK_MASK) &&
	    chip->urb_bytes_max < 73728)
		chip->urb_bytes_max = 73728;
	chip->rate = 48000;
	chip->mode = &chip->modes[0];
	dev_idx = -1;		/* released by ffuc_card_free() from now on */

	strscpy(card->driver, "FirefaceUSB", sizeof(card->driver));
	strscpy(card->shortname, desc->shortname, sizeof(card->shortname));
	strscpy(card->mixername, desc->shortname, sizeof(card->mixername));
	if (!id[chip->dev_idx])
		snd_card_set_id(card, desc->card_id);

	ffuc_midi_init(chip);
	ret = ffuc_alloc_urbs(chip);
	if (ret < 0)
		goto free_card;

	if (desc->status_iface != FFUC_IFACE_NONE) {
		chip->status_intf = usb_ifnum_to_if(dev, desc->status_iface);
		if (chip->status_intf &&
		    usb_driver_claim_interface(&ffuc_driver, chip->status_intf,
					       NULL) < 0) {
			dev_warn(&intf->dev,
				 "status interface busy, no underrun recovery\n");
			chip->status_intf = NULL;
		}
	}

	ffuc_read_identity(chip);
	{
		u32 st = 0;

		if (ffuc_vendor_recv(chip, FFUC_REQ_STATUS, 0, 0, &st,
				     sizeof(st)) == 0)
			dev_info(&intf->dev,
				 "probe status 0x%08x rate index %u (device state before any write)\n",
				 le32_to_cpu(st), le32_to_cpu(st) >> 24);
	}
	snprintf(card->longname, sizeof(card->longname),
		 "RME %s (%s, firmware %u) at %s",
		 desc->name,
		 chip->iso ? "ISO transport" : "interrupt transport",
		 chip->firmware, dev_name(&dev->dev));

	ret = ffuc_hw_setup(chip);
	if (ret < 0) {
		dev_err(&intf->dev, "hardware init failed: %d\n", ret);
		goto free_card;
	}
	ret = ffuc_create_pcm(chip);
	if (ret < 0)
		goto free_card;
	ret = ffuc_hwdep_create(chip);
	if (ret < 0)
		goto free_card;
	ret = ffuc_midi_create(chip);
	if (ret < 0)
		dev_warn(&intf->dev, "no MIDI port: %d\n", ret);
	ret = ffuc_transport_setup(chip);
	if (ret < 0)
		dev_warn(&intf->dev, "raw control transports unavailable: %d\n",
			 ret);
	ret = snd_card_register(card);
	if (ret < 0)
		goto free_card;

	usb_set_intfdata(intf, chip);
	if (chip->status_intf && ffuc_status_submit(chip) < 0)
		dev_warn(&intf->dev, "cannot watch the status endpoint\n");
	dev_info(&intf->dev, "%s, card %d, %u frames x %u URBs\n",
		 card->longname, card->number, chip->base_urb_frames,
		 chip->nurbs);
	return 0;

free_card:
	if (chip->status_intf) {
		usb_driver_release_interface(&ffuc_driver, chip->status_intf);
		chip->status_intf = NULL;
	}
	snd_card_free(card);
release_idx:
	if (dev_idx >= 0) {
		mutex_lock(&devices_mutex);
		clear_bit(dev_idx, &devices_used);
		mutex_unlock(&devices_mutex);
	}
	return ret;
}

static void ffuc_disconnect(struct usb_interface *intf)
{
	struct ffuc *chip = usb_get_intfdata(intf);

	if (!chip)
		return;
	usb_set_intfdata(intf, NULL);

	snd_card_disconnect(chip->card);
	debugfs_remove_recursive(chip->debugfs);
	chip->debugfs = NULL;
	mutex_lock(&chip->mutex);
	WRITE_ONCE(chip->disconnected, true);
	WRITE_ONCE(chip->engine_running, false);
	ffuc_kill_urbs(chip, true);
	mutex_unlock(&chip->mutex);
	cancel_work_sync(&chip->recover_work);
	ffuc_hwdep_cleanup(chip);
	ffuc_midi_cleanup(chip);
	if (chip->status_intf) {
		usb_driver_release_interface(&ffuc_driver, chip->status_intf);
		chip->status_intf = NULL;
	}
	snd_card_free_when_closed(chip->card);
}

static int ffuc_suspend(struct usb_interface *intf, pm_message_t message)
{
	struct ffuc *chip = usb_get_intfdata(intf);

	if (!chip)
		return 0;
	snd_power_change_state(chip->card, SNDRV_CTL_POWER_D3hot);
	snd_pcm_suspend_all(chip->pcm);
	cancel_work_sync(&chip->recover_work);
	mutex_lock(&chip->mutex);
	ffuc_engine_stop(chip);
	chip->prepared_mask = 0;
	mutex_unlock(&chip->mutex);
	ffuc_midi_suspend(chip);
	ffuc_transport_suspend(chip);
	return 0;
}

static int ffuc_resume(struct usb_interface *intf)
{
	struct ffuc *chip = usb_get_intfdata(intf);
	int ret;

	if (!chip)
		return 0;
	mutex_lock(&chip->mutex);
	ret = ffuc_hw_setup(chip);
	mutex_unlock(&chip->mutex);
	if (ret == 0)
		ffuc_transport_setup(chip);
	ffuc_midi_resume(chip);
	snd_power_change_state(chip->card, SNDRV_CTL_POWER_D0);
	return ret;
}

static struct usb_driver ffuc_driver = {
	.name = "snd-fireface-uc",
	.probe = ffuc_probe,
	.disconnect = ffuc_disconnect,
	.suspend = ffuc_suspend,
	.resume = ffuc_resume,
	.reset_resume = ffuc_resume,
	.id_table = ffuc_ids,
	.supports_autosuspend = 0,
};

module_usb_driver(ffuc_driver);

MODULE_DESCRIPTION("RME Fireface UC/UCX USB audio transport (PCM + hwdep)");
MODULE_LICENSE("GPL");
