//! Enumerate every Fireface visible through the kernel hwdep ABI.
//!
//! Several units can be present at once; the kernel driver creates one
//! card + one hwdep control node per physical device.  This example prints
//! the card number, model, transport and endpoints so a caller can pass a
//! specific card to [`Fireface::open_card`].

#[cfg(feature = "alsa")]
fn main() {
    use tuxmix_core::fireface::{class_compliant_cards, Fireface};

    let devices = Fireface::enumerate();
    if devices.is_empty() {
        println!("No Fireface hwdep device found (is snd-fireface-uc loaded?)");
    }
    for d in devices {
        println!(
            "card {}: {} ({}) serial={} firmware={} rate={} channels={} inputs={} \
             outputs={} playback={} meter={}B state_ep=0x{:02x} meter_ep=0x{:02x}",
            d.card,
            d.name,
            if d.iso { "ISO" } else { "interrupt" },
            if d.serial.is_empty() { "-" } else { &d.serial },
            d.firmware,
            d.rate,
            d.channels,
            d.inputs,
            d.outputs,
            d.playback,
            d.meter_bytes,
            d.state_ep,
            d.meter_ep
        );
    }

    for c in class_compliant_cards() {
        println!(
            "card {}: {} is in Class Compliant (UAC2) mode — no RME mixer control; \
             use the stock ALSA/PipeWire device",
            c.card, c.name
        );
    }
}

#[cfg(not(feature = "alsa"))]
fn main() {
    eprintln!("build with --features alsa");
}
