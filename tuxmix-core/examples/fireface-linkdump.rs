//! Dump raw 0x85 transport/link packets (FFUC_EV_METER) with timestamps.
//!
//!   cargo run -p tuxmix-core --example fireface-linkdump --features alsa -- 2 10
//!
//! Read-only: opens the hwdep node, which starts the persistent 0x85 reader,
//! and prints every packet as hex.  Used to compare device link telemetry
//! before and after a live sample-rate change.

#[cfg(feature = "alsa")]
fn main() {
    use std::time::{Duration, Instant};
    use tuxmix_core::fireface::{Direction, Hwdep};

    let mut args = std::env::args().skip(1);
    let card: i32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(2);
    let seconds: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(10);

    let hw = Hwdep::open(card).expect("open hwdep");
    let info = hw.info().expect("info");
    eprintln!(
        "card {}: {} meter={}B",
        card,
        info.name_str(),
        info.meter_bytes
    );
    let _ = hw.vendor(0x11, Direction::In, 0, 0, &[0u8; 4]);

    let start = Instant::now();
    let deadline = start + Duration::from_secs(seconds);
    while Instant::now() < deadline {
        match hw.wait(Duration::from_millis(200)) {
            Ok(true) => match hw.read_event() {
                Ok(e) => {
                    let ms = start.elapsed().as_millis();
                    print!("{ms}\t{}\t{}", e.kind, e.data.len());
                    for b in &e.data {
                        print!(" {b:02x}");
                    }
                    println!();
                }
                Err(e) => {
                    eprintln!("read event: {e}");
                    break;
                }
            },
            Ok(false) => {}
            Err(e) => {
                eprintln!("poll: {e}");
                break;
            }
        }
    }
}

#[cfg(not(feature = "alsa"))]
fn main() {
    eprintln!("build with --features alsa");
}
