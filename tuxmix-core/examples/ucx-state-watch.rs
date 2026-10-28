//! Print raw UCX state/status events for a short window.
//!
//!   cargo run -p tuxmix-core --example ucx-state-watch --features alsa -- 2 20
//!
//! Intended for hardware bring-up.  It opens the hwdep node directly and
//! prints changed IF1 state words and any status packets.

#[cfg(feature = "alsa")]
fn main() {
    use std::collections::HashMap;
    use std::time::{Duration, Instant};
    use tuxmix_core::fireface::abi::{EV_STATE, EV_STATUS};
    use tuxmix_core::fireface::Hwdep;

    let mut args = std::env::args().skip(1);
    let card: i32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(2);
    let seconds: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(20);

    let hw = Hwdep::open(card).expect("open hwdep");
    println!("watching card {card} for {seconds}s");
    let mut seen: HashMap<u16, u16> = HashMap::new();
    let mut heartbeat: u16 = 0;
    let mut next_heartbeat = Instant::now();
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < deadline {
        let now = Instant::now();
        if now >= next_heartbeat {
            // The vendor driver keeps the IF1 link alive with 0x3f00 = 0..15.
            if let Err(e) = hw.write_words(&[(0x3f00, heartbeat)]) {
                eprintln!("heartbeat: {e}");
                break;
            }
            heartbeat = (heartbeat + 1) & 0x0f;
            next_heartbeat = now + Duration::from_millis(100);
        }
        if !hw.wait(Duration::from_millis(20)).unwrap_or(false) {
            continue;
        }
        let Ok(event) = hw.read_event() else { continue };
        match event.kind {
            EV_STATE => {
                for word in event.data.chunks_exact(4) {
                    let raw = u32::from_le_bytes([word[0], word[1], word[2], word[3]]);
                    let addr = ((raw >> 16) & 0x7fff) as u16;
                    let value = raw as u16;
                    if seen.get(&addr) == Some(&value) {
                        continue;
                    }
                    seen.insert(addr, value);
                    println!("STATE addr=0x{addr:04x} value=0x{value:04x} ({value})");
                }
            }
            EV_STATUS => {
                let hex: Vec<String> = event.data.iter().map(|b| format!("{b:02x}")).collect();
                println!("STATUS seq={} {}", event.seq, hex.join(" "));
            }
            _ => {}
        }
    }
}

#[cfg(not(feature = "alsa"))]
fn main() {
    eprintln!("build with --features alsa");
}
