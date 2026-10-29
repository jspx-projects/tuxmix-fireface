# Driver, USB and latency architecture

Orientation note for the Fireface UCX path. It shows where audio is buffered
between the application and the converters, how the class-compliant driver
and `snd-fireface-uc` differ, and how the measured latency numbers relate to
the TuxMix latency line.

## 1. Host and device layers

```text
 DAW / TuxMix / any ALSA client
        |
        |  application buffer (DAW block / PipeWire quantum)
        v
 PipeWire graph  (quantum, periods, graph scheduling)
        |
        |  ALSA PCM client
        v
 +----------------------+          +-----------------------------+
 | snd-usb-audio        |          | snd-fireface-uc             |
 | (class-compliant)    |          | (vendor protocol)           |
 |                      |          |                             |
 | URB queue            |          | URB queue                   |
 | UAC feedback         |          | fb_delay + out_lead         |
 | no vendor 60 lead    |          | 60-frame UCX lead           |
 +----------+-----------+          +--------------+--------------+
            |                                     |
            +----------------+--------------------+
                             |
                    USB core / HCD
                             |
                    USB 2.0 cable
                             |
                    UCX firmware
                      DSP / mixer
                      DAC / ADC
```

The UCX exposes three mutually exclusive audio personalities:

| Personality | Host driver | USB transport | Mixer control |
|---|---|---|---|
| Normal RME, macOS/Linux | `snd-fireface-uc` | IF0 isochronous | TuxMix / hwdep |
| Normal RME, Windows | RME Windows driver | IF5 interrupt | TotalMix FX |
| Class Compliant | `snd-usb-audio` | UAC2 isochronous | stored mixer only |

Only one personality is active at a time. The class driver and the vendor
driver share the ALSA/PipeWire layers above them, but not the USB transport or
the mixer control plane.

## 2. Buffers

| Buffer | Owner | Typical control | Notes |
|---|---|---|---|
| Application block | DAW / PipeWire | ASIO buffer / quantum | Smallest host buffer the app requests |
| ALSA ring | `snd-fireface-uc` / `snd-usb-audio` | period count and driver floor | Must cover the USB queue and device lead |
| USB URB queue | USB driver | `nurbs`, `urb_frames`, feedback delay | ISO packets; one URB is normally 1 ms at 1x |
| Device output lead | UCX firmware | 60 frames in the vendor path | Applies after the USB queue |
| Converters | UCX hardware | fixed | DAC on output, ADC on input |

The vendor path currently uses:

```text
fb_delay = (nurbs + 1) * iso_packets
lead     = fb_delay * rate / 8000 + 60
ring     = round_up(max((nurbs + 2) * urb_frames,
                        lead + 2 * urb_frames), urb_frames)
```

The last line is the buffer-lead fix. It exists because the old 192-frame
floor at 96 frames was smaller than the 204-frame output lead and underran.

## 3. Latency budget

The latency line is an application/ALSA budget. The cable probe measures the
device path and does not fill the host ring, so the two numbers must not be
compared directly.

At 48 kHz / 128 frames with the default queue:

| Term | Frames | Time |
|---|---|---|
| Host ALSA ring, old floor | 256 | 5.33 ms |
| USB queue | 144 | 3.00 ms |
| UCX device lead | 60 | 1.25 ms |
| Output converter half | 13 | 0.27 ms |
| **Latency-line total, old floor** | | **9.85 ms** |
| Measured cable path | | **4.48 ms** |

After the buffer-lead fix the idle floor is 336 frames, so a 128-frame graph
uses 384 frames (8.0 ms) and the line becomes about 12.5 ms. That is
deliberately conservative: 128 frames was already above the 204-frame lead.
The fix mainly protects 96 and below. The tuned 16/4 queue keeps the 128
frame ring at 256 frames.

With `urb_frames=16`, `nurbs=4` at 48 kHz / 128 frames:

```text
ring       5.33 ms
USB queue  1.875 ms   (5 URBs of 18 frames)
lead       1.25 ms
converter  0.27 ms
total      8.72 ms
measured cable path: 3.83 ms median, 3.47 ms mean
```

The MR18 class-compliant reference measured **3.32 ms** on the same cable
probe. Its class driver has UAC feedback and no 60-frame vendor lead. The
MR18 descriptor exposes a sync endpoint, so its feedback is explicit rather
than implicit.

## 4. Where the latency is spent

1. **Host ring** — usually the largest term and controlled by the graph
   quantum/periods. At 48 kHz, 128 frames costs 5.33 ms; 64 frames costs
   2.67 ms before driver floors.
2. **USB queue / feedback delay** — 3.0 ms stock, 1.875 ms with the tuned
   queue at 48 kHz. This is driver policy, not USB wire time.
3. **UCX device lead** — 60 frames, 1.25 ms at 48 kHz, 0.63 ms at 96 kHz,
   0.31 ms at 192 kHz. This is vendor/device behaviour.
4. **Converters** — about 25 frames round trip (0.52 ms at 48 kHz). The
   cable-minus-internal measurement isolates this; the output line uses half.
5. **Scheduling/start phase** — moves a single measurement by up to one USB
   frame; it cancels in cable-minus-internal and is not part of the budget.

The class driver removes the 60-frame vendor lead and uses its own UAC
feedback/queue; that is part of why the MR18 can be lower on the same
Linux/PipeWire stack. The MR18's sync endpoint indicates explicit feedback,
not implicit feedback. The remaining UCX margin is a device property, not an
ALSA setting.

### What implicit feedback means

A USB Audio asynchronous playback endpoint needs the host to adapt its send
rate to the device clock. The normal mechanism is an explicit feedback
endpoint that reports the required rate. Some devices omit that endpoint; in
that case `snd-usb-audio` can use the **capture stream** as the reference:
playback URBs are queued from capture URB completion and the captured packet
sizes are copied into the playback packets.

That is implicit feedback. It is a UAC timing mechanism, not the same as the
`lowlatency` URB-requeue mode. The `snd-usb-audio` module has an
`implicit_fb` override for devices with broken descriptors; the MR18 is not
an implicit-feedback example because its descriptor exposes a sync endpoint.

## 5. Experiment summary

All measurements use a cable from AN3 out to AN3 in, with AN4 as the internal
digital reference, unless stated otherwise.

| Experiment | Result |
|---|---|
| Stock UCX, 48/128 | cable median 4.48 ms; internal 3.88 ms; delta 0.604 ms |
| `urb_frames=16`, `nurbs=4`, lead 60 | cable median 3.83 ms, mean 3.47 ms |
| lead 30 | 3.34 ms median but 3.87 ms mean and outliers; not adopted |
| lead 0 | 4.11 ms median; worse |
| `fb_extra_urb=0` | mostly silent; reject |
| 96 kHz single tone | clean after the lead-aware buffer fix |
| 18-channel load, 48 kHz, tuned 16/4 | clean |
| 18-channel load, 96 kHz, tuned 16/4 | fails |
| 18-channel load, 96 kHz, stock 48/2 | clean, period 96 / buffer 608 |
| 192 kHz tuned 16/4 | clicks at 64 and 128 periods |
| 192 kHz stock 48/2 | clean, period 192 / buffer 1024 |
| IF5 interrupt path | card enumerates, but capture is silent; not usable yet |
| Class Compliant mode | not measured; loses TotalMix host control |

The safe configuration today is:

```text
48 kHz:      tuned 16/4 is usable for low latency
96/192 kHz:  stock 48/2 is required for 18-channel load
```

The main branch carries the buffer-lead fix with the stock queue. The tuned
queue remains an experimental branch setting.

## 6. Code map

| Concern | Location |
|---|---|
| Device descriptors, transports, URB engines | `kernel/ffuc.c` |
| ISO queue, `fb_delay`, `out_lead` | `kernel/ffuc.c: ffuc_iso_setup_urbs()` |
| ALSA period/buffer constraints | `kernel/ffuc.c: ffuc_rule_period()`, `ffuc_rule_buffer()` |
| URB completion/resubmit | `kernel/ffuc.c: ffuc_urb_complete()` |
| IF5 interrupt path | `kernel/ffuc.c: ffuc_int_setup_urbs()` |
| Driver queue readout | `ui/tuxmix/tuxmix-core/src/pw_clock.rs` |
| Latency line and idle buffer estimate | `ui/tuxmix/tuxmix-gui/src/app.rs` |
| Latency model in the specification | `docs/spec/TUXMIX-SPEC.md` §12 |

## 7. Measurement caveat

The probe starts ALSA streams directly and measures the cable impulse. It
does not fill a steady-state application ring. Treat its result as the
device/USB/converter path, then add the host ring for a DAW-like total. The
MR18 and UCX numbers in this note use the same probe, so their device paths
are comparable; the application totals are only comparable at the same
quantum and buffer policy.
