//! Read-only hwdep event monitor.
//!
//! Prints event counts and payload sizes for a card over a short window.
//! Useful for confirming the raw transports without touching mixer state.
//!
//!   cargo run -p tuxmix-core --example fireface-events --features alsa -- 2 3

#[cfg(feature = "alsa")]
fn main() {
    use std::time::{Duration, Instant};
    use tuxmix_core::fireface::{Direction, Hwdep};

    let mut args = std::env::args().skip(1);
    let card: i32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(2);
    let seconds: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(3);

    let hw = Hwdep::open(card).expect("open hwdep");
    let info = hw.info().expect("info");
    println!(
        "card {}: {} ({}) abi={} model={} transport={} meter={}B",
        card,
        info.name_str(),
        info.serial_str(),
        info.abi,
        info.model,
        info.transport,
        info.meter_bytes
    );
    if let Ok(c) = hw.counters() {
        println!(
            "counters: out_urbs={} out_gaps={} out_shorts={} out_status_errors={} urb_errors={} recoveries={} events_dropped={} meter_urbs={} state_urbs={} status_urbs={}",
            c.out_urbs, c.out_gaps, c.out_shorts, c.out_status_errors,
            c.urb_errors, c.recoveries, c.events_dropped,
            c.meter_urbs, c.state_urbs, c.status_urbs
        );
    }

    // Read-only status/firmware reads.
    for (name, req) in [("status", 0x11u8), ("firmware", 0x1c)] {
        match hw.vendor(req, Direction::In, 0, 0, &[0u8; 4]) {
            Ok(d) if d.len() == 4 => println!(
                "{name}: {:02x} {:02x} {:02x} {:02x}",
                d[0], d[1], d[2], d[3]
            ),
            Ok(d) => println!("{name}: {} bytes", d.len()),
            Err(e) => println!("{name}: {e}"),
        }
    }

    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut counts = std::collections::BTreeMap::new();
    let mut status_shown = 0usize;
    while Instant::now() < deadline {
        match hw.wait(Duration::from_millis(200)) {
            Ok(true) => match hw.read_event() {
                Ok(e) => {
                    if e.kind == 3 && status_shown < 40 {
                        status_shown += 1;
                        println!(
                            "status seq={} {:02x} {:02x} {:02x} {:02x}",
                            e.seq,
                            e.data.first().copied().unwrap_or(0),
                            e.data.get(1).copied().unwrap_or(0),
                            e.data.get(2).copied().unwrap_or(0),
                            e.data.get(3).copied().unwrap_or(0)
                        );
                    }
                    let key = (e.kind, e.data.len());
                    *counts.entry(key).or_insert(0usize) += 1;
                }
                Err(e) => {
                    println!("read event: {e}");
                    break;
                }
            },
            Ok(false) => {}
            Err(e) => {
                println!("poll: {e}");
                break;
            }
        }
    }
    for ((kind, len), count) in counts {
        println!("event type {kind}: {count} x {len} bytes");
    }
    if let Ok(c) = hw.counters() {
        println!(
            "end counters: out_urbs={} meter_urbs={} state_urbs={} status_urbs={} events_dropped={}",
            c.out_urbs, c.meter_urbs, c.state_urbs, c.status_urbs, c.events_dropped
        );
    }
}

#[cfg(not(feature = "alsa"))]
fn main() {
    eprintln!("build with --features alsa");
}
