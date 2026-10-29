# UCX latency measurement

Working note. Measures the UCX device path with the internal digital loopback
and with a physical output-to-input cable, and relates both to the latency
line's formula.

## Method

- TuxMix and PipeWire are stopped; the ALSA node is opened directly as
  `hw:2,0`.
- Playback channel 1 (PCM1) is routed to DSP outputs 2 and 3 (AN3 and AN4).
  AN3 output is set to a safe level (-20 dB); AN4 output is at 0 dB. Inputs
  AN3 and AN4 are at 0 dB gain, +4 dBu, unmuted.
- The internal loopback is enabled on AN4 only (`0x15` index 3). The cable
  runs from AN3 output to AN3 input, so AN3 carries only the cable path.
- Capture channels 2 and 3 are recorded in the same run: channel 2 is the
  cable path, channel 3 is the internal loopback. Both share the playback
  stream, the USB queue, the device lead and the capture stream, so the
  difference between them isolates the converter path and the cable.
- The playback signal is a full-scale impulse every 1024 frames after a
  512-frame pre-roll. The capture is grouped by phase modulo the train;
  the densest phase cluster is the impulse train. This rejects cable
  ringing and reflections, which otherwise bias a simple peak average.
- The delay is corrected by the PCM trigger timestamps. The trigger
  correction is below 0.03 ms.

The absolute delay varies with the ALSA start phase and with the playback
buffer occupancy at the impulse. The cable-minus-internal difference cancels
both, and is the reliable quantity.

## Temporary probe

`tools/alsa-latency-probe.c` is a temporary measurement tool. It is not
part of the shipped tooling and is not in the default build. Build it by hand:

```text
cc -O2 -o /tmp/alsa-latency-probe tools/alsa-latency-probe.c -lasound -lm
/tmp/alsa-latency-probe hw:2,0 48000 128 96 0 2 3 18
```

Arguments: device, rate, period, periods, playback channel, capture channel,
optional second capture channel, channel count (all channels are zero-based).
The channel count is 18 for the UC/UCX; the probe itself is a generic ALSA
impulse-train tool and works with any PCM that accepts S32_LE at the requested
channel count. The probe only plays and records; the caller sets the routing,
levels and loopback, and must free the ALSA node first. Remove or replace the
probe once the latency line is settled.

## Results

### Converter path (cable minus internal)

| Rate | Periods tested | Difference | Frames |
|---|---|---|---|
| 44.1 kHz | 48, 64, 96, 128, 256 | 0.567 ms | 25 |
| 48 kHz | 48, 64, 96, 128, 256 | 0.521 ms | 25 |
| 96 kHz | 96, 128, 256 | 0.260 ms | 25 |
| 192 kHz | 192, 256 | 0.120 ms | 23 |

Every repeated run at a given rate produced the same difference; period size
does not affect it. The converter round-trip is therefore a fixed 25 frames
at 1x and 2x, and 23 frames at 4x. It is the DAC + ADC group delay, not a
host or USB scheduling effect.

### Absolute internal-loopback delay

| Rate | Range over runs and periods | Design device side (queue + lead) |
|---|---|---|
| 44.1 kHz | 3.56 - 6.61 ms | 4.63 ms |
| 48 kHz | 3.13 - 6.61 ms | 4.25 ms |
| 96 kHz | 3.08 - 4.63 ms | 3.63 ms |
| 192 kHz | 3.01 - 3.80 ms | 3.31 ms |

The absolute value moves in steps of one USB frame with the ALSA start phase
and the playback buffer occupancy. It is a transient latency, not the
steady-state ring budget used by the latency line. The design device side is
the upper bound of the measured range; the minimum is reached when the
impulse leaves on the next USB frame.

## Delay sources

| Source | Where it is counted | Value |
|---|---|---|
| Host graph ring | latency line `buffer`, output side | `max(2 × quantum, ceil((nurbs + 2) × frames_per_urb / quantum) × quantum)` |
| USB queue | latency line `USB` | `(nurbs + 1) × frames_per_urb`; 3.0 ms at 1x, 2x and 4x |
| Device output lead | latency line `device lead` | 60 frames |
| DAC + ADC | output line uses half; round trip uses both | 25 frames at 1x/2x, 23 at 4x (measured) |
| Input USB queue and lead | not in the output latency line | mirrors the output queue on a round trip |
| Host capture ring | not in the output latency line | one graph buffer on a round trip |
| Cable propagation | included in the DAC + ADC row | below 1 us, negligible |
| ALSA start phase | measurement only | up to one USB frame; cancels in the cable minus internal difference |

## Formula

The latency line is an output-side budget:

```text
output ≈ ring + (nurbs + 1) × frames_per_urb + 60 frames
         + 13 frames            (1x and 2x, output DAC half)
         + 12 frames            (4x, output DAC half)
```

For a full output-to-input round trip, the capture side adds a second ring,
the input USB queue and lead, and the converters:

```text
round trip ≈ 2 × (ring + (nurbs + 1) × frames_per_urb + 60 frames)
           + 25 frames            (1x and 2x)
           + 23 frames            (4x)
```

The converter term is measured on the UCX and assumed to be the same on the UC.
The doubled host/USB chain is a model: the loopback measurement confirms the
converter term, but it does not fill the host rings, so it cannot confirm the
steady-state doubled chain.

## Open checks

- Repeat the absolute measurement with a deliberately full playback ring, so
  the transient matches the latency line's steady-state budget.
- Measure the UC USB path; the converter figure is assumed to match the UCX.
- Recheck the 192 kHz converter figure, which is 23 frames rather than 25.
