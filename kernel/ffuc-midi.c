// SPDX-License-Identifier: GPL-2.0-only
/*
 * RME Fireface USB - MIDI over the class-compliant USB-MIDI interface.
 *
 * The unit exposes a standard USB-MIDI 1.0 streaming interface (interface 2
 * on the UCX) with bulk OUT 0x07 and bulk IN 0x86.  MIDI arrives as 4-byte
 * event packets: cable number in the high nibble of byte 0, code index number
 * in the low nibble, then one to three MIDI bytes.  Both directions use cable
 * 0, which is the wired MIDI input and output.
 *
 * The vendor driver owns the device, so this file claims the streaming
 * interface and republishes it as one ALSA rawmidi port.  The port is only
 * polled while its substream is open.
 */

#include <linux/delay.h>
#include <linux/slab.h>
#include <linux/usb.h>
#include <linux/usb/audio.h>
#include <sound/core.h>
#include <sound/rawmidi.h>

#include "ffuc.h"

#define FFUC_MIDI_CABLE		0
#define FFUC_MIDI_PACKET	4
#define FFUC_MIDI_IN_BYTES	512
#define FFUC_MIDI_OUT_BYTES	192
#define FFUC_MIDI_OUT_PACKETS	(FFUC_MIDI_OUT_BYTES + 2)
#define FFUC_MIDI_OUT_INTERVAL	1	/* ms between output polls */
#define FFUC_MIDI_OUT_TIMEOUT	1000	/* ms per bulk transfer */

/* Code index number -> number of MIDI bytes carried in the packet. */
static const u8 ffuc_midi_cin_len[16] = {
	0, 0, 2, 3, 3, 1, 2, 3,
	3, 3, 3, 3, 2, 2, 3, 1,
};

static void ffuc_midi_in_complete(struct urb *urb)
{
	struct ffuc *chip = urb->context;
	unsigned int i;

	if (urb->status) {
		if (urb->status != -ENOENT && urb->status != -ECONNRESET &&
		    urb->status != -ESHUTDOWN && urb->status != -EPROTO)
			dev_dbg_ratelimited(&chip->dev->dev,
					    "MIDI IN status %d\n", urb->status);
		return;
	}

	for (i = 0; i + FFUC_MIDI_PACKET <= urb->actual_length;
	     i += FFUC_MIDI_PACKET) {
		const u8 *pkt = urb->transfer_buffer + i;
		unsigned int cin = pkt[0] & 0x0f;
		unsigned int cable = pkt[0] >> 4;
		unsigned int len = ffuc_midi_cin_len[cin];

		if (cable != FFUC_MIDI_CABLE || !len)
			continue;
		if (READ_ONCE(chip->midi_in_sub))
			snd_rawmidi_receive(chip->midi_in_sub, pkt + 1, len);
	}

	if (READ_ONCE(chip->midi_in_running))
		usb_submit_urb(urb, GFP_ATOMIC);
}

static void ffuc_midi_in_start(struct ffuc *chip)
{
	unsigned int i;
	int ret;

	if (!chip->midi_intf || READ_ONCE(chip->disconnected) ||
	    READ_ONCE(chip->midi_in_running))
		return;

	WRITE_ONCE(chip->midi_in_running, true);
	for (i = 0; i < FFUC_MIDI_IN_URBS; i++) {
		struct urb *urb = chip->midi_in_urb[i];

		usb_fill_bulk_urb(urb, chip->dev,
				  usb_rcvbulkpipe(chip->dev,
						  chip->desc->midi_in_ep),
				  chip->midi_in_buf[i], FFUC_MIDI_IN_BYTES,
				  ffuc_midi_in_complete, chip);
		ret = usb_submit_urb(urb, GFP_ATOMIC);
		if (ret < 0) {
			dev_dbg(&chip->dev->dev,
				"MIDI IN submit failed: %d\n", ret);
			WRITE_ONCE(chip->midi_in_running, false);
			break;
		}
	}
}

static void ffuc_midi_in_stop(struct ffuc *chip)
{
	unsigned int i;

	WRITE_ONCE(chip->midi_in_running, false);
	for (i = 0; i < FFUC_MIDI_IN_URBS; i++)
		usb_kill_urb(chip->midi_in_urb[i]);
}

/* -- host to unit ---------------------------------------------------- */

static unsigned int ffuc_midi_emit(u8 *out, unsigned int npkt,
				   unsigned int cin, const u8 *data,
				   unsigned int len)
{
	u8 *pkt = out + npkt * FFUC_MIDI_PACKET;

	pkt[0] = (FFUC_MIDI_CABLE << 4) | cin;
	pkt[1] = data[0];
	pkt[2] = len > 1 ? data[1] : 0;
	pkt[3] = len > 2 ? data[2] : 0;
	return npkt + 1;
}

/* Convert a raw MIDI byte stream into USB-MIDI event packets.  Partial
 * messages are kept in chip->midi_tx across calls. */
static unsigned int ffuc_midi_pack(struct ffuc *chip, const u8 *in,
				   unsigned int len, u8 *out)
{
	struct ffuc_midi_tx *tx = &chip->midi_tx;
	unsigned int i, npkt = 0;

	for (i = 0; i < len; i++) {
		u8 b = in[i];
		unsigned int need, cin;

		if (b >= 0xf8) {	/* realtime, may interleave */
			npkt = ffuc_midi_emit(out, npkt, 0xf, &b, 1);
			continue;
		}
		if (b == 0xf0) {
			tx->in_sysex = true;
			tx->sysex_len = 0;
			tx->msg_len = 0;
			tx->sysex[tx->sysex_len++] = b;
			continue;
		}
		if (b == 0xf7) {
			if (tx->in_sysex && tx->sysex_len) {
				tx->sysex[tx->sysex_len++] = b;
				npkt = ffuc_midi_emit(out, npkt, 4 + tx->sysex_len,
						      tx->sysex, tx->sysex_len);
				tx->sysex_len = 0;
			} else {
				npkt = ffuc_midi_emit(out, npkt, 0xf, &b, 1);
			}
			tx->in_sysex = false;
			continue;
		}
		if (tx->in_sysex) {
			tx->sysex[tx->sysex_len++] = b;
			if (tx->sysex_len == 3) {
				npkt = ffuc_midi_emit(out, npkt, 4, tx->sysex, 3);
				tx->sysex_len = 0;
			}
			continue;
		}
		if (b & 0x80) {
			if (b == 0xf6) {	/* tune request: one byte */
				npkt = ffuc_midi_emit(out, npkt, 0xf, &b, 1);
				continue;
			}
			tx->status = b;
			tx->msg_len = 0;
			continue;
		}
		if (!tx->status)
			continue;	/* stray data byte */
		tx->msg[tx->msg_len++] = b;
		if ((tx->status & 0xf0) == 0xf0) {
			/* system common: F1/F3 take one data byte, F2 two */
			need = tx->status == 0xf2 ? 2 : 1;
			cin = tx->status == 0xf2 ? 3 : 2;
		} else {
			need = ((tx->status & 0xf0) == 0xc0 ||
				(tx->status & 0xf0) == 0xd0) ? 1 : 2;
			cin = tx->status >> 4;
		}
		if (tx->msg_len >= need) {
			u8 msg[3] = { tx->status, tx->msg[0], tx->msg[1] };

			npkt = ffuc_midi_emit(out, npkt, cin, msg, need + 1);
			tx->msg_len = 0;
		}
	}
	return npkt;
}

static void ffuc_midi_out_work(struct work_struct *work)
{
	struct ffuc *chip = container_of(work, struct ffuc,
					 midi_out_work.work);
	struct snd_rawmidi_substream *substream;
	u8 in[FFUC_MIDI_OUT_BYTES];
	u8 out[FFUC_MIDI_OUT_PACKETS * FFUC_MIDI_PACKET];

	if (READ_ONCE(chip->disconnected) ||
	    !READ_ONCE(chip->midi_out_running))
		return;
	substream = READ_ONCE(chip->midi_out_sub);
	if (!substream)
		return;

	for (;;) {
		unsigned int npkt;
		int n, sent = 0, ret;

		n = snd_rawmidi_transmit_peek(substream, in, sizeof(in));
		if (n <= 0)
			break;
		npkt = ffuc_midi_pack(chip, in, n, out);
		if (npkt) {
			ret = usb_bulk_msg(chip->dev,
					   usb_sndbulkpipe(chip->dev,
							   chip->desc->midi_out_ep),
					   out, npkt * FFUC_MIDI_PACKET, &sent,
					   FFUC_MIDI_OUT_TIMEOUT);
			if (ret < 0) {
				dev_dbg_ratelimited(&chip->dev->dev,
						    "MIDI OUT failed: %d\n", ret);
				break;
			}
		}
		snd_rawmidi_transmit_ack(substream, n);
	}

	if (READ_ONCE(chip->midi_out_running))
		schedule_delayed_work(&chip->midi_out_work,
				      msecs_to_jiffies(FFUC_MIDI_OUT_INTERVAL));
}

/* -- ALSA rawmidi ---------------------------------------------------- */

static int ffuc_midi_input_open(struct snd_rawmidi_substream *substream)
{
	struct ffuc *chip = substream->rmidi->private_data;

	WRITE_ONCE(chip->midi_in_sub, substream);
	if (atomic_inc_return(&chip->midi_in_users) == 1)
		ffuc_midi_in_start(chip);
	return 0;
}

static int ffuc_midi_input_close(struct snd_rawmidi_substream *substream)
{
	struct ffuc *chip = substream->rmidi->private_data;

	if (atomic_dec_and_test(&chip->midi_in_users)) {
		ffuc_midi_in_stop(chip);
		WRITE_ONCE(chip->midi_in_sub, NULL);
	}
	return 0;
}

static void ffuc_midi_input_trigger(struct snd_rawmidi_substream *substream,
				    int up)
{
	/* The URBs follow open/close; several clients may share the
	 * substream and a second trigger must not resubmit live URBs. */
}

static int ffuc_midi_output_open(struct snd_rawmidi_substream *substream)
{
	struct ffuc *chip = substream->rmidi->private_data;

	memset(&chip->midi_tx, 0, sizeof(chip->midi_tx));
	WRITE_ONCE(chip->midi_out_sub, substream);
	return 0;
}

static int ffuc_midi_output_close(struct snd_rawmidi_substream *substream)
{
	struct ffuc *chip = substream->rmidi->private_data;

	WRITE_ONCE(chip->midi_out_running, false);
	cancel_delayed_work_sync(&chip->midi_out_work);
	WRITE_ONCE(chip->midi_out_sub, NULL);
	return 0;
}

static void ffuc_midi_output_trigger(struct snd_rawmidi_substream *substream,
				     int up)
{
	struct ffuc *chip = substream->rmidi->private_data;

	if (up) {
		WRITE_ONCE(chip->midi_out_running, true);
		schedule_delayed_work(&chip->midi_out_work, 0);
	} else {
		WRITE_ONCE(chip->midi_out_running, false);
		cancel_delayed_work_sync(&chip->midi_out_work);
	}
}

static void ffuc_midi_output_drain(struct snd_rawmidi_substream *substream)
{
	struct ffuc *chip = substream->rmidi->private_data;

	if (!READ_ONCE(chip->midi_out_running))
		return;
	flush_delayed_work(&chip->midi_out_work);
}

static const struct snd_rawmidi_ops ffuc_midi_input_ops = {
	.open = ffuc_midi_input_open,
	.close = ffuc_midi_input_close,
	.trigger = ffuc_midi_input_trigger,
};

static const struct snd_rawmidi_ops ffuc_midi_output_ops = {
	.open = ffuc_midi_output_open,
	.close = ffuc_midi_output_close,
	.trigger = ffuc_midi_output_trigger,
	.drain = ffuc_midi_output_drain,
};

/* -- setup / teardown ------------------------------------------------ */

void ffuc_midi_init(struct ffuc *chip)
{
	INIT_DELAYED_WORK(&chip->midi_out_work, ffuc_midi_out_work);
	atomic_set(&chip->midi_in_users, 0);
}

static bool ffuc_midi_interface(const struct usb_interface *intf)
{
	const struct usb_host_interface *alt = intf->cur_altsetting;
	const struct usb_endpoint_descriptor *ep;
	bool in = false, out = false;
	int i;

	if (alt->desc.bInterfaceClass != USB_CLASS_AUDIO ||
	    alt->desc.bInterfaceSubClass != USB_SUBCLASS_MIDISTREAMING)
		return false;
	for (i = 0; i < alt->desc.bNumEndpoints; i++) {
		ep = &alt->endpoint[i].desc;
		if (usb_endpoint_is_bulk_in(ep))
			in = true;
		else if (usb_endpoint_is_bulk_out(ep))
			out = true;
	}
	return in && out;
}

int ffuc_midi_create(struct ffuc *chip)
{
	struct snd_rawmidi *rmidi;
	struct usb_interface *intf;
	unsigned int i;
	int ret;

	if (!chip->desc->midi_out_ep || !chip->desc->midi_in_ep)
		return 0;
	intf = usb_ifnum_to_if(chip->dev, chip->desc->midi_iface);
	if (!intf || !ffuc_midi_interface(intf))
		return -ENODEV;

	if (ffuc_claim_interface(intf) < 0)
		return -EBUSY;
	chip->midi_intf = intf;

	for (i = 0; i < FFUC_MIDI_IN_URBS; i++) {
		chip->midi_in_urb[i] = usb_alloc_urb(0, GFP_KERNEL);
		chip->midi_in_buf[i] = kmalloc(FFUC_MIDI_IN_BYTES, GFP_KERNEL);
		if (!chip->midi_in_urb[i] || !chip->midi_in_buf[i]) {
			ret = -ENOMEM;
			goto err_release;
		}
	}

	char id[32];

	strscpy(id, chip->desc->shortname, sizeof(id));
	ret = snd_rawmidi_new(chip->card, id, 0, 1, 1, &rmidi);
	if (ret < 0)
		goto err_release;

	rmidi->private_data = chip;
	rmidi->info_flags = SNDRV_RAWMIDI_INFO_OUTPUT |
			    SNDRV_RAWMIDI_INFO_INPUT |
			    SNDRV_RAWMIDI_INFO_DUPLEX;
	snprintf(rmidi->name, sizeof(rmidi->name), "%s MIDI 1",
		 chip->desc->name);
	snd_rawmidi_set_ops(rmidi, SNDRV_RAWMIDI_STREAM_INPUT,
			    &ffuc_midi_input_ops);
	snd_rawmidi_set_ops(rmidi, SNDRV_RAWMIDI_STREAM_OUTPUT,
			    &ffuc_midi_output_ops);
	chip->rmidi = rmidi;
	return 0;

err_release:
	for (i = 0; i < FFUC_MIDI_IN_URBS; i++) {
		usb_free_urb(chip->midi_in_urb[i]);
		chip->midi_in_urb[i] = NULL;
		kfree(chip->midi_in_buf[i]);
		chip->midi_in_buf[i] = NULL;
	}
	ffuc_release_interface(intf);
	chip->midi_intf = NULL;
	return ret;
}

void ffuc_midi_cleanup(struct ffuc *chip)
{
	unsigned int i;

	WRITE_ONCE(chip->midi_in_running, false);
	WRITE_ONCE(chip->midi_out_running, false);
	cancel_delayed_work_sync(&chip->midi_out_work);
	for (i = 0; i < FFUC_MIDI_IN_URBS; i++) {
		if (chip->midi_in_urb[i]) {
			usb_kill_urb(chip->midi_in_urb[i]);
			usb_free_urb(chip->midi_in_urb[i]);
			chip->midi_in_urb[i] = NULL;
		}
		kfree(chip->midi_in_buf[i]);
		chip->midi_in_buf[i] = NULL;
	}
	WRITE_ONCE(chip->midi_in_sub, NULL);
	WRITE_ONCE(chip->midi_out_sub, NULL);
	chip->rmidi = NULL;
	if (chip->midi_intf) {
		ffuc_release_interface(chip->midi_intf);
		chip->midi_intf = NULL;
	}
}

void ffuc_midi_suspend(struct ffuc *chip)
{
	WRITE_ONCE(chip->midi_out_running, false);
	cancel_delayed_work_sync(&chip->midi_out_work);
	ffuc_midi_in_stop(chip);
}

void ffuc_midi_resume(struct ffuc *chip)
{
	if (!chip->midi_intf || READ_ONCE(chip->disconnected))
		return;
	if (READ_ONCE(chip->midi_in_sub))
		ffuc_midi_in_start(chip);
	if (READ_ONCE(chip->midi_out_sub)) {
		WRITE_ONCE(chip->midi_out_running, true);
		schedule_delayed_work(&chip->midi_out_work, 0);
	}
}
