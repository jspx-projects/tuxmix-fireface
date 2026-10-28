// SPDX-License-Identifier: GPL-2.0-or-later
/*
 * RME Fireface USB - snd-hwdep control transport.
 *
 * One hwdep node per card exposes:
 *
 *   ioctl(INFO)          identity, capabilities, endpoint numbers
 *   ioctl(VENDOR)        one EP0 vendor request (read or write, with data)
 *   ioctl(WRITE_WORDS)   one or more IF1 register words (parity added here)
 *   ioctl(COUNTERS)      transport health counters
 *   read()/poll()        meter, IF1 state and status events
 *
 * The IF1 meter (0x85) and state (0x8d) endpoints are read continuously by
 * this driver; userspace receives raw packets/words and owns the parsing,
 * because meter layouts and register semantics are device-model policy.
 */
#include <linux/kfifo.h>
#include <linux/module.h>
#include <linux/poll.h>
#include <linux/slab.h>
#include <linux/string.h>
#include <linux/uaccess.h>
#include <linux/usb.h>
#include <sound/core.h>
#include <sound/hwdep.h>

#include "ffuc.h"

/* -- event queue ----------------------------------------------------- */

static bool ffuc_event_ready(struct ffuc *chip)
{
	struct ffuc_hwdep_event_hdr h;
	unsigned long flags;
	bool ready = false;

	spin_lock_irqsave(&chip->event_lock, flags);
	if (kfifo_out_peek(&chip->event_fifo, &h, sizeof(h)) == sizeof(h) &&
	    kfifo_len(&chip->event_fifo) >= sizeof(h) + h.size)
		ready = true;
	spin_unlock_irqrestore(&chip->event_lock, flags);
	return ready;
}

void ffuc_event_put(struct ffuc *chip, u32 type, const void *data, u32 size,
		    u32 flags)
{
	struct ffuc_hwdep_event_hdr h;
	unsigned long irqflags;

	if (!chip->hwdep || READ_ONCE(chip->disconnected))
		return;
	if (size > FFUC_EVENT_MAX)
		size = FFUC_EVENT_MAX;

	h.type = type;
	h.size = size;
	h.seq = atomic_inc_return(&chip->event_seq);
	h.flags = flags;

	spin_lock_irqsave(&chip->event_lock, irqflags);
	if (kfifo_avail(&chip->event_fifo) < sizeof(h) + size) {
		atomic_inc(&chip->events_dropped);
		spin_unlock_irqrestore(&chip->event_lock, irqflags);
		return;
	}
	kfifo_in(&chip->event_fifo, &h, sizeof(h));
	if (size)
		kfifo_in(&chip->event_fifo, data, size);
	spin_unlock_irqrestore(&chip->event_lock, irqflags);
	wake_up_interruptible(&chip->event_wait);
}

static ssize_t ffuc_hwdep_read(struct snd_hwdep *hwdep, char __user *buf,
			       long count, loff_t *offset)
{
	struct ffuc *chip = hwdep->private_data;
	struct ffuc_hwdep_event_hdr h;
	u8 *tmp;
	unsigned int len, got;
	unsigned long flags;
	int ret;

	if (count < (long)sizeof(h))
		return -EINVAL;
	if (!ffuc_event_ready(chip)) {
		ret = wait_event_interruptible(chip->event_wait,
					       ffuc_event_ready(chip) ||
					       READ_ONCE(chip->disconnected));
		if (ret)
			return ret;
	}
	if (READ_ONCE(chip->disconnected))
		return -ENODEV;

	tmp = kmalloc(FFUC_EVENT_BUF_SIZE, GFP_KERNEL);
	if (!tmp)
		return -ENOMEM;

	spin_lock_irqsave(&chip->event_lock, flags);
	if (kfifo_out_peek(&chip->event_fifo, &h, sizeof(h)) != sizeof(h) ||
	    kfifo_len(&chip->event_fifo) < sizeof(h) + h.size) {
		spin_unlock_irqrestore(&chip->event_lock, flags);
		kfree(tmp);
		return -EAGAIN;
	}
	len = sizeof(h) + h.size;
	if (count < (long)len) {
		spin_unlock_irqrestore(&chip->event_lock, flags);
		kfree(tmp);
		return -EINVAL;
	}
	got = kfifo_out(&chip->event_fifo, tmp, len);
	spin_unlock_irqrestore(&chip->event_lock, flags);
	if (got != len) {
		kfree(tmp);
		return -EIO;
	}

	if (copy_to_user(buf, tmp, len))
		ret = -EFAULT;
	else
		ret = len;
	kfree(tmp);
	return ret;
}

static __poll_t ffuc_hwdep_poll(struct snd_hwdep *hwdep, struct file *file,
				poll_table *wait)
{
	struct ffuc *chip = hwdep->private_data;
	__poll_t mask = 0;

	poll_wait(file, &chip->event_wait, wait);
	if (READ_ONCE(chip->disconnected))
		return EPOLLERR | EPOLLHUP;
	if (ffuc_event_ready(chip))
		mask |= EPOLLIN | EPOLLRDNORM;
	return mask;
}

static int ffuc_hwdep_open(struct snd_hwdep *hwdep, struct file *file)
{
	struct ffuc *chip = hwdep->private_data;

	if (atomic_inc_return(&chip->hwdep_users) == 1)
		ffuc_transport_start(chip);
	return 0;
}

static int ffuc_hwdep_release(struct snd_hwdep *hwdep, struct file *file)
{
	struct ffuc *chip = hwdep->private_data;

	if (atomic_dec_and_test(&chip->hwdep_users))
		ffuc_transport_stop(chip);
	return 0;
}

/* -- ioctls ---------------------------------------------------------- */

static int ffuc_hwdep_info(struct ffuc *chip,
			   struct ffuc_hwdep_info __user *arg)
{
	struct ffuc_hwdep_info info;
	u32 caps = FFUC_CAP_VENDOR | FFUC_CAP_PCM;

	if (chip->desc->state_out_ep)
		caps |= FFUC_CAP_WORDS;
	if (chip->desc->meter_in_ep && chip->desc->meter_len)
		caps |= FFUC_CAP_METERS;
	if (chip->desc->status_iface != FFUC_IFACE_NONE)
		caps |= FFUC_CAP_STATUS;

	memset(&info, 0, sizeof(info));
	info.abi = FFUC_ABI_VERSION;
	info.model = chip->model;
	info.transport = chip->iso ? FFUC_XPORT_ISO : FFUC_XPORT_INTERRUPT;
	info.caps = caps;
	info.channels = chip->desc->max_channels;
	info.inputs = chip->desc->max_channels;
	info.outputs = chip->desc->max_channels;
	info.playback = chip->desc->max_channels;
	info.matrix_sources = chip->desc->max_channels * 2;
	info.meter_bytes = chip->desc->meter_len;
	info.state_ep = chip->desc->state_in_ep;
	info.meter_ep = chip->desc->meter_in_ep;
	info.status_ep = chip->desc->status_ep;
	info.rate = chip->rate;
	info.firmware = chip->firmware;
	info.card = chip->card->number;
	info.pcm_device = 0;
	strscpy(info.name, chip->desc->name, sizeof(info.name));
	strscpy(info.serial, chip->serial, sizeof(info.serial));

	if (copy_to_user(arg, &info, sizeof(info)))
		return -EFAULT;
	return 0;
}

static int ffuc_hwdep_vendor(struct ffuc *chip,
			     struct ffuc_hwdep_vendor __user *arg)
{
	struct ffuc_hwdep_vendor v;
	void *data = NULL;
	int ret;

	if (copy_from_user(&v, arg, sizeof(v)))
		return -EFAULT;
	if (v.direction > 1)
		return -EINVAL;
	if (v.length > FFUC_HWDEP_MAX_DATA)
		return -EINVAL;

	if (v.length) {
		data = kmalloc(v.length, GFP_KERNEL);
		if (!data)
			return -ENOMEM;
		if (!v.direction)
			memcpy(data, v.data, v.length);
	}

	mutex_lock(&chip->ctl_lock);
	if (v.direction)
		ret = ffuc_vendor_recv(chip, v.request, v.value, v.index,
				       data, v.length);
	else
		ret = ffuc_vendor_send(chip, v.request, v.value, v.index,
				       data, v.length);
	mutex_unlock(&chip->ctl_lock);

	if (ret >= 0 && v.direction && v.length)
		memcpy(v.data, data, v.length);
	kfree(data);
	if (ret < 0)
		return ret;
	if (copy_to_user(arg, &v, sizeof(v)))
		return -EFAULT;
	return 0;
}

static int ffuc_hwdep_words(struct ffuc *chip,
			    struct ffuc_hwdep_words __user *arg)
{
	struct ffuc_hwdep_words w;

	if (copy_from_user(&w, arg, sizeof(w)))
		return -EFAULT;
	if (w.count > FFUC_HWDEP_MAX_WORDS)
		return -EINVAL;
	/*
	 * ffuc_reg_send() owns chip->ctl_lock.  Do not take it here:
	 * the mutex is not recursive, and the UCX IF1 path would deadlock
	 * in the ioctl before the first word reaches the device.
	 */
	return ffuc_reg_send(chip, w.words, w.count);
}

static int ffuc_hwdep_counters(struct ffuc *chip,
			       struct ffuc_hwdep_counters __user *arg)
{
	struct ffuc_hwdep_counters c;

	memset(&c, 0, sizeof(c));
	c.out_urbs = atomic_read(&chip->out_urbs);
	c.out_gaps = atomic_read(&chip->out_gaps);
	c.out_shorts = atomic_read(&chip->out_shorts);
	c.out_status_errors = atomic_read(&chip->out_status_errors);
	c.urb_errors = atomic_read(&chip->urb_errors);
	c.recoveries = chip->recoveries;
	c.events_dropped = atomic_read(&chip->events_dropped);
	c.meter_urbs = atomic_read(&chip->meter_urbs);
	c.state_urbs = atomic_read(&chip->state_urbs);
	c.status_urbs = 0;
	if (copy_to_user(arg, &c, sizeof(c)))
		return -EFAULT;
	return 0;
}

static int ffuc_hwdep_ioctl(struct snd_hwdep *hwdep, struct file *file,
			    unsigned int cmd, unsigned long arg)
{
	struct ffuc *chip = hwdep->private_data;

	if (READ_ONCE(chip->disconnected))
		return -ENODEV;

	switch (cmd) {
	case FFUC_HWDEP_IOCTL_INFO:
		return ffuc_hwdep_info(chip, (void __user *)arg);
	case FFUC_HWDEP_IOCTL_VENDOR:
		return ffuc_hwdep_vendor(chip, (void __user *)arg);
	case FFUC_HWDEP_IOCTL_WRITE_WORDS:
		return ffuc_hwdep_words(chip, (void __user *)arg);
	case FFUC_HWDEP_IOCTL_COUNTERS:
		return ffuc_hwdep_counters(chip, (void __user *)arg);
	default:
		return -ENOTTY;
	}
}

static const struct snd_hwdep_ops ffuc_hwdep_ops = {
	.open = ffuc_hwdep_open,
	.release = ffuc_hwdep_release,
	.read = ffuc_hwdep_read,
	.poll = ffuc_hwdep_poll,
	.ioctl = ffuc_hwdep_ioctl,
};

int ffuc_hwdep_create(struct ffuc *chip)
{
	struct snd_hwdep *hwdep;
	int ret;

	ret = snd_hwdep_new(chip->card, "Fireface", 0, &hwdep);
	if (ret < 0)
		return ret;
	hwdep->private_data = chip;
	hwdep->iface = SNDRV_HWDEP_IFACE_USB_STREAM;
	hwdep->ops = ffuc_hwdep_ops;
	strscpy(hwdep->name, "Fireface Control", sizeof(hwdep->name));
	chip->hwdep = hwdep;
	return 0;
}

/* -- IF1 meter/state transports -------------------------------------- */

static void ffuc_state_complete(struct urb *urb)
{
	struct ffuc *chip = urb->context;
	unsigned int i, words;

	atomic_inc(&chip->state_urbs);
	if (urb->status == 0 && urb->actual_length >= sizeof(u32)) {
		words = urb->actual_length / sizeof(u32);
		/* Split large reads into event-sized chunks. */
		for (i = 0; i < words; i += FFUC_EVENT_MAX / sizeof(u32)) {
			unsigned int n = min_t(unsigned int,
					       words - i,
					       FFUC_EVENT_MAX / sizeof(u32));

			ffuc_event_put(chip, FFUC_EV_STATE,
				       (const u8 *)urb->transfer_buffer +
				       i * sizeof(u32),
				       n * sizeof(u32), 0);
		}
	}

	switch (urb->status) {
	case -ENOENT:
	case -ECONNRESET:
	case -ESHUTDOWN:
	case -ENODEV:
		return;
	default:
		break;
	}
	if (chip->state_active && !READ_ONCE(chip->disconnected))
		schedule_delayed_work(&chip->state_work, msecs_to_jiffies(4));
}

static void ffuc_meter_complete(struct urb *urb)
{
	struct ffuc *chip = urb->context;

	atomic_inc(&chip->meter_urbs);
	if (urb->status == 0 && urb->actual_length >= 4)
		ffuc_event_put(chip, FFUC_EV_METER, urb->transfer_buffer,
			       min_t(unsigned int, urb->actual_length,
				     FFUC_EVENT_MAX), 0);

	switch (urb->status) {
	case -ENOENT:
	case -ECONNRESET:
	case -ESHUTDOWN:
	case -ENODEV:
		return;
	default:
		break;
	}
	if (chip->state_active && !READ_ONCE(chip->disconnected))
		schedule_delayed_work(&chip->meter_work, msecs_to_jiffies(4));
}

static void ffuc_state_work_fn(struct work_struct *work)
{
	struct ffuc *chip = container_of(to_delayed_work(work), struct ffuc,
					 state_work);
	int ret;

	if (!chip->state_active || READ_ONCE(chip->disconnected))
		return;
	ret = usb_submit_urb(chip->state_urb, GFP_KERNEL);
	if (ret < 0 && ret != -EALREADY && chip->state_active)
		schedule_delayed_work(&chip->state_work, msecs_to_jiffies(10));
}

static void ffuc_meter_work_fn(struct work_struct *work)
{
	struct ffuc *chip = container_of(to_delayed_work(work), struct ffuc,
					 meter_work);
	int ret;

	if (!chip->state_active || READ_ONCE(chip->disconnected))
		return;
	ret = usb_submit_urb(chip->meter_urb, GFP_KERNEL);
	if (ret < 0 && ret != -EALREADY && chip->state_active)
		schedule_delayed_work(&chip->meter_work, msecs_to_jiffies(10));
}

static int ffuc_state_submit(struct ffuc *chip)
{
	int ret;

	if (!chip->state_urb)
		return 0;
	ret = usb_submit_urb(chip->state_urb, GFP_KERNEL);
	if (!ret)
		atomic_inc(&chip->state_urbs);
	return ret;
}

static int ffuc_meter_submit(struct ffuc *chip)
{
	int ret;

	if (!chip->meter_urb)
		return 0;
	ret = usb_submit_urb(chip->meter_urb, GFP_KERNEL);
	if (!ret)
		atomic_inc(&chip->meter_urbs);
	return ret;
}

int ffuc_transport_setup(struct ffuc *chip)
{
	const struct ffuc_device_desc *d = chip->desc;
	struct usb_host_interface *alt;
	unsigned int i;
	bool have_out = false, have_in = false, have_meter = false;

	if (d->state_iface == FFUC_IFACE_NONE)
		return 0;

	if (!chip->state_intf) {
		chip->state_intf = usb_ifnum_to_if(chip->dev, d->state_iface);
		if (!chip->state_intf)
			return -ENODEV;
		if (ffuc_claim_interface(chip->state_intf) < 0) {
			chip->state_intf = NULL;
			return -EBUSY;
		}
	}

	alt = chip->state_intf->cur_altsetting;
	for (i = 0; i < alt->desc.bNumEndpoints; i++) {
		struct usb_endpoint_descriptor *ep = &alt->endpoint[i].desc;

		if (usb_endpoint_is_bulk_out(ep) &&
		    ep->bEndpointAddress == d->state_out_ep)
			have_out = true;
		if (usb_endpoint_is_bulk_in(ep) &&
		    ep->bEndpointAddress == d->state_in_ep)
			have_in = true;
		if (usb_endpoint_is_bulk_in(ep) &&
		    ep->bEndpointAddress == d->meter_in_ep)
			have_meter = true;
	}

	if (!have_out && !have_in && !have_meter)
		return -ENODEV;

	if (!chip->transport_works_init) {
		INIT_DELAYED_WORK(&chip->state_work, ffuc_state_work_fn);
		INIT_DELAYED_WORK(&chip->meter_work, ffuc_meter_work_fn);
		chip->transport_works_init = true;
	}

	if (have_in && !chip->state_urb) {
		chip->state_urb = usb_alloc_urb(0, GFP_KERNEL);
		chip->state_buf = usb_alloc_coherent(chip->dev,
						     FFUC_STATE_BUF_SIZE,
						     GFP_KERNEL,
						     &chip->state_dma);
		if (chip->state_urb && chip->state_buf) {
			usb_fill_bulk_urb(chip->state_urb, chip->dev,
					  usb_rcvbulkpipe(chip->dev,
							  d->state_in_ep),
					  chip->state_buf, FFUC_STATE_BUF_SIZE,
					  ffuc_state_complete, chip);
			chip->state_urb->transfer_dma = chip->state_dma;
			chip->state_urb->transfer_flags |=
				URB_NO_TRANSFER_DMA_MAP;
		} else {
			usb_free_urb(chip->state_urb);
			chip->state_urb = NULL;
			if (chip->state_buf)
				usb_free_coherent(chip->dev,
						  FFUC_STATE_BUF_SIZE,
						  chip->state_buf,
						  chip->state_dma);
			chip->state_buf = NULL;
		}
	}
	if (have_meter && d->meter_len && !chip->meter_urb) {
		chip->meter_urb = usb_alloc_urb(0, GFP_KERNEL);
		chip->meter_buf = usb_alloc_coherent(chip->dev, d->meter_len,
						     GFP_KERNEL,
						     &chip->meter_dma);
		if (chip->meter_urb && chip->meter_buf) {
			usb_fill_bulk_urb(chip->meter_urb, chip->dev,
					  usb_rcvbulkpipe(chip->dev,
							  d->meter_in_ep),
					  chip->meter_buf, d->meter_len,
					  ffuc_meter_complete, chip);
			chip->meter_urb->transfer_dma = chip->meter_dma;
			chip->meter_urb->transfer_flags |=
				URB_NO_TRANSFER_DMA_MAP;
		} else {
			usb_free_urb(chip->meter_urb);
			chip->meter_urb = NULL;
			if (chip->meter_buf)
				usb_free_coherent(chip->dev, d->meter_len,
						  chip->meter_buf,
						  chip->meter_dma);
			chip->meter_buf = NULL;
		}
	}

	dev_info(&chip->dev->dev,
		 "raw control: IF%d ep out 0x%02x in 0x%02x meter 0x%02x\n",
		 d->state_iface, d->state_out_ep, d->state_in_ep,
		 d->meter_in_ep);
	if (atomic_read(&chip->hwdep_users))
		return ffuc_transport_start(chip);
	return 0;
}

int ffuc_transport_start(struct ffuc *chip)
{
	int ret;

	if (chip->state_active)
		return 0;
	chip->state_active = true;
	/*
	 * The UC's metering is audio-derived.  Its IF1 0x8d/0x85 streams are
	 * link/transport telemetry, and keeping those persistent bulk URBs
	 * running disturbs the PipeWire capture path.  The UC only needs the
	 * EP0 vendor transport and the 0x8b status endpoint.
	 */
	if (!chip->ucx)
		return 0;
	ret = ffuc_state_submit(chip);
	if (ret < 0 && ret != -EALREADY)
		dev_dbg(&chip->dev->dev, "state URB submit: %d\n", ret);
	ret = ffuc_meter_submit(chip);
	if (ret < 0 && ret != -EALREADY)
		dev_dbg(&chip->dev->dev, "meter URB submit: %d\n", ret);
	return 0;
}

void ffuc_transport_stop(struct ffuc *chip)
{
	chip->state_active = false;
	if (chip->transport_works_init) {
		cancel_delayed_work_sync(&chip->state_work);
		cancel_delayed_work_sync(&chip->meter_work);
	}
	if (chip->state_urb)
		usb_kill_urb(chip->state_urb);
	if (chip->meter_urb)
		usb_kill_urb(chip->meter_urb);
	wake_up_interruptible_all(&chip->event_wait);
}

void ffuc_transport_suspend(struct ffuc *chip)
{
	ffuc_transport_stop(chip);
}

void ffuc_hwdep_cleanup(struct ffuc *chip)
{
	ffuc_transport_suspend(chip);
	if (chip->state_urb) {
		usb_free_urb(chip->state_urb);
		chip->state_urb = NULL;
	}
	if (chip->state_buf) {
		usb_free_coherent(chip->dev, FFUC_STATE_BUF_SIZE,
				  chip->state_buf, chip->state_dma);
		chip->state_buf = NULL;
	}
	if (chip->meter_urb) {
		usb_free_urb(chip->meter_urb);
		chip->meter_urb = NULL;
	}
	if (chip->meter_buf) {
		usb_free_coherent(chip->dev, chip->desc->meter_len,
				  chip->meter_buf, chip->meter_dma);
		chip->meter_buf = NULL;
	}
	if (chip->state_intf) {
		ffuc_release_interface(chip->state_intf);
		chip->state_intf = NULL;
	}
	if (kfifo_initialized(&chip->event_fifo)) {
		kfifo_free(&chip->event_fifo);
		memset(&chip->event_fifo, 0, sizeof(chip->event_fifo));
	}
	chip->hwdep = NULL;
}
