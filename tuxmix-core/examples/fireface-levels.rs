//! Print UC input and playback levels from the PipeWire taps.
//!
//!   TUXMIX_NO_CAPTURE=1 cargo run -p tuxmix-core --example fireface-levels --features alsa -- 2 4
//!
//! Run while audio plays through the RME Pro Audio sink.  Input levels come
//! from the PipeWire source tap; playback levels come from the sink monitor.

#[cfg(feature = "alsa")]
fn main() {
    use std::time::{Duration, Instant};
    use tuxmix_core::fireface::Fireface;
    use tuxmix_core::RmeDevice;
    use tuxmix_core::pw_meter::PlaybackTap;

    let mut args = std::env::args().skip(1);
    let card: i32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(2);
    let seconds: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(4);

    std::env::set_var("TUXMIX_NO_CAPTURE", "1");
    let mut dev = Fireface::open_card(card).expect("open Fireface");
    let tap = PlaybackTap::start();
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut last = Instant::now();

    while Instant::now() < deadline {
        if last.elapsed() < Duration::from_millis(100) {
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }
        last = Instant::now();
        let _ = dev.poll_events();
        let inputs = dev.meters().unwrap_or_default();
        let (playback, pch) = tap.drain();

        let fmt = |v: &[f32], n: usize| {
            v.iter()
                .take(n)
                .enumerate()
                .map(|(i, x)| format!("{i}{:+.1}", 20.0 * x.max(1e-9).log10()))
                .collect::<Vec<_>>()
                .join(" ")
        };
        println!(
            "in[{}] pb[{} ch={}]",
            fmt(&inputs, 18),
            fmt(&playback, 18),
            pch
        );
    }
}

#[cfg(not(feature = "alsa"))]
fn main() {
    eprintln!("build with --features alsa");
}
