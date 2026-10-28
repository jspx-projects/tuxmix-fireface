//! Dump raw UC `0x85` packets and report changing 32-bit words.
//!
//!   cargo run -p tuxmix-core --example uc-meter-watch --features alsa -- 3 20
//!
//! The UC packet is 324 bytes (81 words).  This is a hardware probe for the
//! unresolved pre-loopback meter layout.

#[cfg(feature = "alsa")]
fn main() {
    use std::collections::HashMap;
    use std::time::{Duration, Instant};
    use tuxmix_core::fireface::abi::EV_METER;
    use tuxmix_core::fireface::Hwdep;

    let mut args = std::env::args().skip(1);
    let card: i32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(3);
    let seconds: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(20);
    let hw = Hwdep::open(card).expect("open hwdep");
    let info = hw.info().expect("info");
    println!(
        "card {card}: {} meter_len={} for {seconds}s",
        info.name_str(),
        info.meter_bytes
    );

    let mut previous: HashMap<usize, u32> = HashMap::new();
    let mut packets = 0u64;
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < deadline {
        if !hw.wait(Duration::from_millis(200)).unwrap_or(false) {
            continue;
        }
        let Ok(event) = hw.read_event() else { continue };
        if event.kind != EV_METER {
            continue;
        }
        packets += 1;
        for (idx, word) in event.data.chunks_exact(4).enumerate() {
            let value = u32::from_le_bytes([word[0], word[1], word[2], word[3]]);
            if let Some(previous) = previous.get(&idx) {
                if *previous != value {
                    println!("word {idx:02} {previous:#010x} -> {value:#010x} ({previous} -> {value})");
                }
            } else {
                previous.insert(idx, value);
            }
        }
        if packets % 50 == 0 {
            let nonzero: Vec<String> = event
                .data
                .chunks_exact(4)
                .enumerate()
                .filter_map(|(i, b)| {
                    let v = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                    (v != 0).then(|| format!("{i}:{v}"))
                })
                .collect();
            println!("packet {packets} len={} nonzero={}", event.data.len(), nonzero.join(" "));
        }
    }
    println!("packets {packets}");
}

#[cfg(not(feature = "alsa"))]
fn main() {
    eprintln!("build with --features alsa");
}
