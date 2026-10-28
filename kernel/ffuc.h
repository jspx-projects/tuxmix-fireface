/* SPDX-License-Identifier: GPL-2.0-only */
/*
 * RME Fireface USB audio driver - core definitions.
 *
 * Design (2026-10-25 refactor)
 * ----------------------------
 * The kernel module is intentionally a *transport* driver, not a mixer:
 *
 *   - it enumerates the USB device, selects the streaming alternate setting,
 *     runs the URB engine and exposes the ALSA PCM device;
 *   - it runs the device's raw control transports (EP0 vendor requests and,
 *     where the device has one, the interface-1 bulk register/meter channel);
 *   - it exposes all of that through one snd-hwdep node per card;
 *   - it contains a table of per-device descriptors so a single module binds
 *     to several RME models and to several units at once.
 *
 * Everything that is policy or a device model - matrix routing, faders, pan,
 * preamps, EQ/dynamics, clock preferences, meters, scenes - is implemented
 * in userspace (TuxMix) on top of the hwdep ABI in ffuc-uapi.h.  Do not add
 * model-specific ALSA controls here; add a userspace profile instead.
 *
 * Transport summary
 * -----------------
 * Fireface UC PC personality (0424:3fc5, interface 0):
 *   interrupt endpoints 0x01 OUT / 0x82 IN (0x03/0x84 at 4x), interleaved
 *   float32 frames, no header.  The status ticker on interface 5 (0x8b)
 *   reports front-panel events and the output-latch bit.
 *
 * Fireface UC Mac personality (0424:3fc6 / 2a39:3fc6):
 *   isochronous endpoints 0x01/0x82; IN packets are a 4-byte header plus
 *   planar frames, OUT packets are frames with a per-frame lead word.
 *
 * Fireface UCX (0424:3fc9 / 2a39:3fc9):
 *   interface 0 has the ISO endpoints 0x0e OUT / 0x8f IN; interface 5 has
 *   the Windows interrupt endpoints 0x01/0x82.  The module parameter
 *   ucx_if5 chooses which personality binds (default ISO IF0).
 */
#ifndef FFUC_H
#define FFUC_H

#include <linux/kfifo.h>
#include <linux/mutex.h>
#include <linux/poll.h>
#include <linux/spinlock.h>
#include <linux/usb.h>
#include <linux/wait.h>
#include <linux/workqueue.h>
#include <sound/core.h>
#include <sound/hwdep.h>
#include <sound/pcm.h>
#include <sound/rawmidi.h>

#include "ffuc-uapi.h"

#define FFUC_VENDOR_SMSC	0x0424
#define FFUC_VENDOR_RME		0x2a39
#define FFUC_PRODUCT_UC_PC	0x3fc5
#define FFUC_PRODUCT_UC_MAC	0x3fc6
#define FFUC_PRODUCT_UCX	0x3fc9

#define FFUC_IFACE_NONE		0xff

#define FFUC_STATUS_LEN		4
#define FFUC_STATUS_OUT_LATCHED	0x80
#define FFUC_STATUS_GRACE_MS	20

#define FFUC_MAX_EPS		2	/* endpoints per direction (alt 3) */
#define FFUC_MAX_URBS		32
#define FFUC_MAX_CHANNELS	18
#define FFUC_MAX_RATE		768000
#define FFUC_SAMPLE_BYTES	4
#define FFUC_MAX_URB_ERRORS	16
#define FFUC_MIN_QUEUE_FRAMES	64
#define FFUC_MIDI_IN_URBS	2	/* bulk IN URBs kept queued */
#define FFUC_CTL_TIMEOUT	1000	/* ms */

/* ISO ("Mac"/IF0) transport. */
#define FFUC_MAX_ISO_PACKETS	8	/* per URB, one per 125 us microframe */
#define FFUC_ISO_IN_HEADER	4
#define FFUC_ISO_COUNTER_MOD	0x2000
#define FFUC_FB_SIZE		1024
#define FFUC_ISO_FRAME_MASK	0x7ff

#define FFUC_STATE_BUF_SIZE	1024
#define FFUC_EVENT_FIFO_SIZE	(64 * 1024)
#define FFUC_EVENT_BUF_SIZE	(sizeof(struct ffuc_hwdep_event_hdr) + \
				 FFUC_EVENT_MAX)

/* Vendor request numbers, shared by the UC and UCX.  The meaning of some
 * numbers differs between models; see docs/PROTOCOL-COMPLETE.md.
 */
#define FFUC_REQ_SETTINGS	0x10
#define FFUC_REQ_STATUS		0x11
#define FFUC_REQ_MIXER		0x12
#define FFUC_REQ_MUTE		0x13
#define FFUC_REQ_UNMUTE		0x14
#define FFUC_REQ_LOOPBACK	0x15
#define FFUC_REQ_CHAN_INIT	0x16
#define FFUC_REQ_LEVELS		0x17
#define FFUC_REQ_PGA		0x1a
#define FFUC_REQ_DDS		0x1b
#define FFUC_REQ_REVISION	0x1c
#define FFUC_REQ_ENABLE		0x20

/* UCX/Windows path: request 16/19/20/21/22/23/27/29 and the status reads
 * 28/30/31.  The numbers are documented with the protocol, not repeated
 * here. */
#define FFUC_REQ_UCX_INIT22	0x16
#define FFUC_REQ_UCX_MUTE	0x13
#define FFUC_REQ_UCX_UNMUTE	0x14
#define FFUC_REQ_UCX_LOOPBACK	0x15
#define FFUC_REQ_UCX_STREAM29	0x1d
#define FFUC_REQ_UCX_FIRMWARE	0x1c
#define FFUC_REQ_UCX_SERIAL_LO	0x1e
#define FFUC_REQ_UCX_SERIAL_HI	0x1f

/* quirks */
#define FFUC_QUIRK_ALT_RESELECT		BIT(0)	/* alt 0 then alt N at start */
#define FFUC_QUIRK_INT32_SAMPLES	BIT(1)	/* IF5 sends S32, not float */
#define FFUC_QUIRK_STATUS_LATCH		BIT(2)	/* status byte 3 bit 7 */
#define FFUC_QUIRK_IF5_PRE_START	BIT(3)	/* 0x10 0x8000 + 0x1d before audio */
#define FFUC_QUIRK_IF5_PLAYBACK_MASK	BIT(4)	/* req 19/20 channel mask */

struct ffuc;

/* One USB bandwidth class (alternate setting). */
struct ffuc_mode {
	unsigned int alt;
	unsigned int neps;		/* endpoints per direction */
	unsigned int words;		/* channels (32-bit words) per endpoint */
	unsigned int pkt_frames;	/* interrupt: frames in one max packet */
	/* ISO only */
	unsigned int in_mps;
	unsigned int out_mps;
	unsigned int max_frames;
};

struct ffuc_urb {
	struct urb *urb;
	struct ffuc_ep *ep;
	void *buf;
	dma_addr_t dma;
};

struct ffuc_ep {
	struct ffuc *chip;
	int dir;
	unsigned int index;
	int next_start;
	struct ffuc_urb urbs[FFUC_MAX_URBS];
};

struct ffuc_stream {
	struct snd_pcm_substream *substream;
	bool running;
	u32 out_counter;
	bool out_counter_valid;
	snd_pcm_uframes_t pos[FFUC_MAX_EPS];
	snd_pcm_uframes_t hw_ptr;
	snd_pcm_uframes_t period;
};

/* Per-model core callbacks.  They may touch the device transport but must
 * not create ALSA controls or implement mixer policy. */
struct ffuc_ops {
	int (*init)(struct ffuc *chip);
	int (*set_rate)(struct ffuc *chip, unsigned int rate);
	int (*stream_start)(struct ffuc *chip, unsigned int channels);
	int (*stream_stop)(struct ffuc *chip, unsigned int channels);
	int (*hw_mute)(struct ffuc *chip, bool mute);
};

struct ffuc_device_desc {
	const char *name;		/* "Fireface UC" */
	const char *shortname;
	const char *card_id;
	u16 vendor;
	u16 product;
	u8 iface;
	u8 transport;			/* enum ffuc_transport */
	u32 quirks;
	u32 model;			/* enum ffuc_model */
	const struct ffuc_mode *modes;
	unsigned int n_modes;
	u8 audio_iface;
	u8 out_ep;
	u8 in_ep;
	u8 status_iface;
	u8 status_ep;
	u8 state_iface;
	u8 state_out_ep;
	u8 state_in_ep;
	u8 meter_in_ep;
	u8 midi_iface;
	u8 midi_out_ep;
	u8 midi_in_ep;
	unsigned int max_channels;
	unsigned int max_rate;
	unsigned int meter_len;
	const struct ffuc_ops *ops;
};

/* USB-MIDI output parser state, kept across rawmidi chunks. */
struct ffuc_midi_tx {
	u8 status;
	u8 msg[2];
	unsigned int msg_len;
	u8 sysex[3];
	unsigned int sysex_len;
	bool in_sysex;
};

struct ffuc {
	struct usb_device *dev;
	struct usb_interface *intf;
	struct snd_card *card;
	struct snd_pcm *pcm;
	struct snd_hwdep *hwdep;

	const struct ffuc_device_desc *desc;
	unsigned int model;
	bool iso;
	bool ucx;
	unsigned int audio_iface;
	unsigned int cur_alt;		/* current streaming alternate setting */
	u8 out_ep, in_ep;
	const struct ffuc_mode *modes;

	struct mutex mutex;		/* engine/rate state */
	struct mutex ctl_lock;		/* serialises EP0/bulk control writes */
	int dev_idx;
	bool disconnected;

	/* engine */
	bool engine_running;
	bool engine_broken;
	/* UC: the next engine stop/start is a rate change and needs the
	 * Windows session-close/open words.  Not used on every start. */
	bool session_reopen;
	/* UC: once a live rate change has happened, every later stream
	 * open/close needs the Windows session trio/close word.  The initial
	 * 48 kHz startup stays untouched. */
	bool uc_session;
	unsigned int hw_params_mask;
	unsigned int prepared_mask;
	unsigned int rate;
	const struct ffuc_mode *mode;
	unsigned int urb_frames;
	unsigned int base_urb_frames;
	unsigned int nurbs;
	size_t urb_bytes_max;
	atomic_t urb_errors;

	/* ISO implicit feedback */
	unsigned int iso_packets;
	spinlock_t fb_lock;
	u32 fb[FFUC_FB_SIZE];
	u64 in_pkts, out_pkts;
	unsigned int in_end;
	unsigned int fb_acc;
	unsigned int fb_delay;
	unsigned int out_lead;
	bool iso_frames_ok;

	/* status ticker (core output-latch watchdog) */
	struct usb_interface *status_intf;
	struct urb *status_urb;
	u8 *status_buf;
	bool status_active;
	unsigned long status_armed_at;
	struct work_struct recover_work;
	unsigned int recoveries;
	u8 last_status[FFUC_STATUS_LEN];
	bool out_latched;

	/* IF1 raw state/meter transports (owned by ffuc-hwdep.c) */
	struct usb_interface *state_intf;
	struct urb *state_urb;
	u8 *state_buf;
	dma_addr_t state_dma;
	struct urb *meter_urb;
	u8 *meter_buf;
	dma_addr_t meter_dma;
	bool state_active;
	bool transport_works_init;
	struct delayed_work state_work;
	struct delayed_work meter_work;
	atomic_t hwdep_users;
	atomic_t meter_urbs;
	atomic_t state_urbs;

	/* USB-MIDI streaming interface (bulk OUT / bulk IN) */
	struct snd_rawmidi *rmidi;
	struct usb_interface *midi_intf;
	struct urb *midi_in_urb[FFUC_MIDI_IN_URBS];
	u8 *midi_in_buf[FFUC_MIDI_IN_URBS];
	struct snd_rawmidi_substream *midi_in_sub;
	struct snd_rawmidi_substream *midi_out_sub;
	struct delayed_work midi_out_work;
	struct ffuc_midi_tx midi_tx;
	atomic_t midi_in_users;
	bool midi_in_running;
	bool midi_out_running;

	/* hwdep events */
	struct kfifo event_fifo;
	spinlock_t event_lock;
	wait_queue_head_t event_wait;
	atomic_t event_seq;
	atomic_t events_dropped;

	/* transport health */
	struct dentry *debugfs;
	atomic_t out_urbs;
	atomic_t out_gaps;
	atomic_t out_shorts;
	atomic_t out_status_errors;
	ktime_t last_out_complete;

	/* identity */
	u32 firmware;
	char serial[32];

	struct ffuc_ep eps[2][FFUC_MAX_EPS];
	struct ffuc_stream streams[2];
};

/* ffuc.c */
int ffuc_vendor_send(struct ffuc *chip, u8 req, u16 val, u16 idx,
		     const void *data, u16 len);
int ffuc_vendor_recv(struct ffuc *chip, u8 req, u16 val, u16 idx,
		     void *data, u16 len);
int ffuc_reg_send(struct ffuc *chip, const struct ffuc_hwdep_word *words,
		  unsigned int count);
int ffuc_claim_interface(struct usb_interface *intf);
void ffuc_release_interface(struct usb_interface *intf);
unsigned int ffuc_rate_from_index(unsigned int idx);

/* ffuc-hwdep.c */
void ffuc_event_put(struct ffuc *chip, u32 type, const void *data, u32 size,
		    u32 flags);
int ffuc_hwdep_create(struct ffuc *chip);
void ffuc_hwdep_cleanup(struct ffuc *chip);
int ffuc_transport_setup(struct ffuc *chip);
int ffuc_transport_start(struct ffuc *chip);
void ffuc_transport_stop(struct ffuc *chip);

/* ffuc-midi.c */
void ffuc_midi_init(struct ffuc *chip);
int ffuc_midi_create(struct ffuc *chip);
void ffuc_midi_cleanup(struct ffuc *chip);
void ffuc_midi_suspend(struct ffuc *chip);
void ffuc_midi_resume(struct ffuc *chip);
void ffuc_transport_suspend(struct ffuc *chip);

#endif /* FFUC_H */
