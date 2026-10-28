//! Live scan of the Fireface UCX TuxMix ALSA backend.
//!
//!     cargo run -p tuxmix-core --example ucx-scan --features alsa

use tuxmix_core::{ChannelId, RmeDevice, UcxAlsa};

fn main() {
    match UcxAlsa::open().or_else(|_| UcxAlsa::open_uc()) {
        Ok(d) => {
            println!("model: {}", d.model_name());
            println!("inputs: {}", d.inputs().len());
            for (i, c) in d.inputs().iter().enumerate() {
                println!(
                    "  in {i:2}: {:<10} type {:?} gain {:?}/{:?} phantom {}",
                    c.name, c.channel_type, c.gain, c.gain_max, c.phantom
                );
            }
            println!(
                "playbacks: {}  outputs: {}  pairs: {}",
                d.playbacks().len(),
                d.outputs().len(),
                d.output_pair_count()
            );
            for (i, p) in d.playbacks().iter().enumerate() {
                println!("  pb {i:2}: {:<12} id {}", p.name, p.id);
            }
            for (i, o) in d.outputs().iter().enumerate() {
                println!(
                    "  out {i:2}: {:<12} vol {:.3} mute {} loop {}",
                    o.name, o.volume, o.mute, o.loopback
                );
            }
            println!(
                "clock: {} sources {:?} rate {}",
                d.settings().clock_source,
                d.settings().clock_sources,
                d.settings().sample_rate
            );
            for (label, cid) in [
                ("input 1", ChannelId::Input(0)),
                ("input 5", ChannelId::Input(4)),
                ("playback 1", ChannelId::Playback(0)),
                ("playback 2", ChannelId::Playback(1)),
            ] {
                if let Ok(v) = d.volume(cid, 0) {
                    println!("{label} -> output pair 0: {v:.3}");
                }
                if let Ok(p) = d.pan(cid, 0) {
                    println!("{label} pan: {p}");
                }
            }
            println!("input meters available: {}", d.meters_available());
            std::thread::sleep(std::time::Duration::from_millis(500));
            if let Some(m) = d.meters() {
                let n = m.len().min(18);
                println!("input meters: {:?}", &m[..n]);
            }
        }
        Err(e) => eprintln!("open failed: {e:?}"),
    }
}
