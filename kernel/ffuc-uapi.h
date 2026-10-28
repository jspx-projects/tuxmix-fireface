/* SPDX-License-Identifier: GPL-2.0-or-later */
/*
 * RME Fireface USB - userspace ABI for the snd-hwdep control transport.
 *
 * The kernel driver is deliberately small: it owns USB enumeration, the
 * streaming engine, the ALSA PCM device and the raw device transports.
 * Everything that is policy or model (matrix, faders, EQ, meters, device
 * profiles, scenes) lives in userspace and talks to the driver through the
 * hwdep node.
 *
 * This header is the single source of truth for the ABI.  The Rust side
 * lives in ui/tuxmix/tuxmix-core/src/fireface/abi.rs and must be updated
 * together with it.
 */
#ifndef _FFUC_UAPI_H
#define _FFUC_UAPI_H

#include <linux/types.h>
#include <linux/ioctl.h>

#define FFUC_ABI_VERSION	2

/* Largest payload in one FFUC_HWDEP_IOCTL_VENDOR request. */
#define FFUC_HWDEP_MAX_DATA	64
/* Largest register burst accepted by FFUC_HWDEP_IOCTL_WRITE_WORDS. */
#define FFUC_HWDEP_MAX_WORDS	63
/* Largest payload in one event record (state, meter or status). */
#define FFUC_EVENT_MAX		512

enum ffuc_model {
	FFUC_MODEL_UNKNOWN	= 0,
	FFUC_MODEL_UC		= 1,
	FFUC_MODEL_UCX		= 2,
};

enum ffuc_transport {
	FFUC_XPORT_INTERRUPT	= 0,	/* UC PC personality (IF5) */
	FFUC_XPORT_ISO		= 1,	/* ISO/class-compliant personality */
};

/* ffuc_hwdep_info.caps */
#define FFUC_CAP_VENDOR		(1u << 0)	/* vendor control transfers */
#define FFUC_CAP_WORDS		(1u << 1)	/* IF1 0x0c register words */
#define FFUC_CAP_METERS		(1u << 2)	/* 0x85 meter events */
#define FFUC_CAP_STATUS		(1u << 3)	/* status endpoint events */
#define FFUC_CAP_PCM		(1u << 4)	/* ALSA PCM device */

enum ffuc_event_type {
	FFUC_EV_METER		= 1,	/* raw 0x85 packet */
	FFUC_EV_STATE		= 2,	/* raw 0x8d words (LE u32) */
	FFUC_EV_STATUS		= 3,	/* raw status endpoint packet */
	FFUC_EV_LATCH		= 4,	/* output-latch transition (UC) */
};

struct ffuc_hwdep_info {
	__u32 abi;
	__u32 model;		/* enum ffuc_model */
	__u32 transport;	/* enum ffuc_transport */
	__u32 caps;
	__u32 channels;		/* maximum channels each way at 1x */
	__u32 inputs;		/* hardware input channels */
	__u32 outputs;		/* hardware output channels */
	__u32 playback;		/* software playback channels */
	__u32 matrix_sources;	/* inputs + playback */
	__u32 meter_bytes;	/* 0 = no meter endpoint */
	__u32 state_ep;
	__u32 meter_ep;
	__u32 status_ep;
	__u32 rate;		/* current rate in Hz */
	__u32 firmware;		/* vendor firmware revision, 0 if unknown */
	__u32 card;		/* ALSA card number */
	__u32 pcm_device;	/* ALSA PCM device number */
	char name[32];		/* e.g. "Fireface UCX" */
	char serial[32];	/* USB serial string, may be empty */
	__u32 reserved[16];
};

/*
 * One vendor request.  direction 0 sends `length` bytes from `data`
 * (usually zero); direction 1 reads into `data` and updates `length` with
 * the number of bytes received.
 */
struct ffuc_hwdep_vendor {
	__u8 request;
	__u8 direction;
	__u16 value;
	__u16 index;
	__u16 length;
	__u16 flags;
	__u8 data[FFUC_HWDEP_MAX_DATA];
};

/* One IF1 register word (address + value); the kernel adds odd parity. */
struct ffuc_hwdep_word {
	__u16 addr;
	__u16 value;
};

struct ffuc_hwdep_words {
	__u32 count;
	__u32 flags;
	struct ffuc_hwdep_word words[FFUC_HWDEP_MAX_WORDS];
};

/* Header of every record returned by read(2). */
struct ffuc_hwdep_event_hdr {
	__u32 type;		/* enum ffuc_event_type */
	__u32 size;		/* payload bytes after this header */
	__u32 seq;		/* monotonically increasing per event */
	__u32 flags;		/* event-type specific */
};

struct ffuc_hwdep_counters {
	__u32 out_urbs;
	__u32 out_gaps;
	__u32 out_shorts;
	__u32 out_status_errors;
	__u32 urb_errors;
	__u32 recoveries;
	__u32 events_dropped;
	__u32 meter_urbs;
	__u32 state_urbs;
	__u32 status_urbs;
};

#define FFUC_HWDEP_IOCTL_INFO \
	_IOR('F', 0x10, struct ffuc_hwdep_info)
#define FFUC_HWDEP_IOCTL_VENDOR \
	_IOWR('F', 0x11, struct ffuc_hwdep_vendor)
#define FFUC_HWDEP_IOCTL_WRITE_WORDS \
	_IOW('F', 0x12, struct ffuc_hwdep_words)
#define FFUC_HWDEP_IOCTL_COUNTERS \
	_IOR('F', 0x13, struct ffuc_hwdep_counters)

#endif /* _FFUC_UAPI_H */
