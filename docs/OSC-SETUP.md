# OSC setup

This note covers enabling and verifying TotalMix FX OSC control. The protocol
itself is defined in [`spec/OSC-CONTROL-SPEC.md`](spec/OSC-CONTROL-SPEC.md).

## TotalMix FX

Open **Options → Settings → OSC** and set:

| Field | Value |
|---|---|
| OSC input port | 7001 |
| Remote output port | 9001 |
| Remote host | the control host |
| OSC enabled | on |
| Send Peak Level Data | optional |
| Peak Hold | optional |

TotalMix FX listens for OSC on its input port and sends feedback to the
configured remote host and port. The feedback source port is selected by the
operating system and is not fixed.

Peak-level feedback adds the `level` paths for the visible channels. It is
useful for a meter display and adds feedback traffic without changing the
control protocol.

The OSC setting is stored with the TotalMix FX state and is applied when the
application starts.

## Network

Both directions use UDP. Allow the TotalMix FX host to send UDP to the control
host's feedback port, and allow the control host to send UDP to the TotalMix FX
input port.

The control host must be reachable from the TotalMix FX host by address. Use
the address of the interface on the same network, not a loopback address.

## Verification

Send a harmless page-1 command and confirm a feedback message:

```text
/setBankStart 0.0
/1/micgain1 0.8
```

The reply uses the same path and carries the resulting value or a `Val`
companion path. A command that changes no state may produce no feedback.

Page changes are explicit. Sending a parameter with a new page prefix makes
TotalMix FX re-send the complete state of that page.

## Operational rules

- Send one command at a time when checking state; toggle commands are
  edge-triggered.
- Use `setBankStart` to select a channel for page 2 rather than relying on the
  page-1 scroll position.
- Keep the control host's firewall open for the feedback port in both UDP
  directions.
- Do not run two OSC controllers with the same feedback port.
