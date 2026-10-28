//! Mechanical UCX acceptance probes.
//!
//! This is a hardware test helper, not a device API.  It opens a UCX through
//! the hwdep backend, performs the probe, and leaves the device on the scene
//! it captured before the probe.
//!
//!   ucx-acceptance info [serial]
//!   ucx-acceptance backup <file> [serial]
//!   ucx-acceptance restore <file> [serial]
//!   ucx-acceptance route [serial]
//!   ucx-acceptance controls <backup-file> [serial]
//!   ucx-acceptance meters [serial]
//!   ucx-acceptance out-eq <backup-file> [serial]
//!
//! `route` routes software playback 1/2 to Phones and opens both output
//! channels.  `controls` exercises gain, phase, Hi-Z, loopback, playback
//! matrix routing, and output level/mute, then restores the captured scene.
//! `meters` samples input, playback, PipeWire tap and post-fader output
//! meters for eight seconds (play a tone through the sink while it runs).
//! `out-eq` writes one UCX hardware-output EQ block, then restores the
//! captured scene.
//!
//! Build:
//!   cargo build -p tuxmix-core --example ucx-acceptance --features alsa

#[cfg(feature = "alsa")]
fn main() {
    use tuxmix_core::fireface::{Fireface, Model};
    use tuxmix_core::{ChannelId, RmeDevice};

    let mut args = std::env::args().skip(1);
    let command = args.next().unwrap_or_else(|| "info".into());
    let first = args.next();
    let serial_arg = match (command.as_str(), first.as_deref()) {
        ("backup" | "restore" | "controls" | "loopback" | "out-eq", Some(_)) => {
            // These commands take a path first; the serial is optional after.
            args.next()
        }
        _ => first.clone(),
    };

    let found = Fireface::enumerate()
        .into_iter()
        .find(|info| {
            info.model == Model::Ucx
                && serial_arg
                    .as_deref()
                    .map_or(true, |serial| info.serial == serial)
        })
        .unwrap_or_else(|| panic!("no matching Fireface UCX"));

    println!(
        "ucx card={} serial={} transport={} firmware={} inputs={} outputs={} playback={}",
        found.card,
        found.serial,
        if found.iso { "ISO" } else { "interrupt" },
        found.firmware,
        found.inputs,
        found.outputs,
        found.playback
    );

    let mut dev = Fireface::open_identity(&found.identity()).expect("open UCX");
    let caps = dev.capabilities();
    println!(
        "caps protocol={} eq={} loopback={} phase={} output_balance={} matrix_cells={} phantom={} pad={}",
        caps.protocol_features,
        caps.eq,
        caps.loopback,
        caps.phase,
        caps.output_balance,
        caps.matrix_cells,
        caps.input_phantom,
        caps.input_pad
    );

    match command.as_str() {
        "info" => {
            println!("PASS info");
        }
        "backup" => {
            let path = first.expect("backup needs a file path");
            let scene = dev.capture_scene();
            std::fs::write(&path, scene.to_json().expect("serialize scene"))
                .expect("write scene backup");
            println!("PASS backup {path}");
        }
        "restore" => {
            let path = first.expect("restore needs a file path");
            let json = std::fs::read_to_string(&path).expect("read scene backup");
            let scene = tuxmix_core::Scene::from_json(&json).expect("parse scene");
            dev.apply_scene(&scene).expect("restore scene");
            println!("PASS restore {path}");
        }
        "route" => {
            // TuxMix output pair 0 is Phones 7/8.  Software playback 1/2 are
            // source channels 0 and 1.  Both go to the pair at unity.
            for playback in 0..2 {
                dev.set_volume(ChannelId::Playback(playback), 0, 1.0)
                    .expect("route playback to Phones");
            }
            for output in 0..2 {
                dev.set_mute(ChannelId::Output(output), false)
                    .expect("unmute Phones");
                dev.set_volume(ChannelId::Output(output), 0, 1.0)
                    .expect("set Phones level");
            }
            dev.set_loopback(0, false).expect("clear Phones loopback");
            println!("PASS route PB1/2 -> Phones");
        }
        "controls" => {
            let backup = first.expect("controls needs a backup file path");
            let before = dev.capture_scene();
            std::fs::write(&backup, before.to_json().expect("serialize scene"))
                .expect("write scene backup");

            dev.set_gain(0, 30).expect("set AN1 gain");
            dev.set_phase(0, true).expect("set AN1 phase");
            dev.set_instrument(2, true).expect("set AN3 Hi-Z");
            dev.set_loopback(0, true).expect("set Phones loopback");
            for playback in 0..2 {
                dev.set_volume(ChannelId::Playback(playback), 0, 1.0)
                    .expect("route playback");
            }
            for output in 0..2 {
                dev.set_mute(ChannelId::Output(output), false)
                    .expect("unmute output");
                dev.set_volume(ChannelId::Output(output), 0, 1.0)
                    .expect("set output level");
            }

            let after = dev.capture_scene();
            assert_eq!(after.inputs[0].gain, Some(30), "AN1 gain not modelled");
            assert!(after.inputs[0].phase, "AN1 phase not modelled");
            assert!(after.inputs[2].instrument, "AN3 Hi-Z not modelled");
            assert!(after.outputs[0].loopback, "Phones loopback not modelled");
            assert!(
                after.playbacks.iter().take(2).all(|p| p.volumes[0] == 1.0),
                "playback routing not modelled"
            );
            assert_eq!(after.outputs[0].volume, 1.0, "Phones volume not modelled");

            dev.apply_scene(&before).expect("restore pre-probe scene");
            println!("PASS controls");
        }
        "out-eq" => {
            let backup = first.expect("out-eq needs a backup file path");
            let before = dev.capture_scene();
            std::fs::write(&backup, before.to_json().expect("serialize scene"))
                .expect("write scene backup");
            let mut eq = dev.outputs()[2].eq.expect("UCX output 2 EQ");
            eq.enabled = true;
            eq.bands[0].band_type = tuxmix_core::channel::EqBandType::Bell;
            eq.bands[0].freq_hz = 1_000;
            eq.bands[0].gain_db = 3.0;
            dev.set_output_eq(2, &eq).expect("write output EQ");
            assert!(dev.outputs()[2].eq.unwrap().enabled);
            dev.apply_scene(&before).expect("restore pre-probe scene");
            println!("PASS output EQ");
        }
        "loopback" => {
            let backup = first.expect("loopback needs a backup file path");
            let before = dev.capture_scene();
            std::fs::write(&backup, before.to_json().expect("serialize scene"))
                .expect("write scene backup");
            // Route PB1/2 into AN1/2 and enable loopback on that output.
            for playback in 0..2 {
                dev.set_volume(ChannelId::Playback(playback), 1, 1.0)
                    .expect("route playback to AN1/2");
            }
            for output in 2..4 {
                dev.set_mute(ChannelId::Output(output), false)
                    .expect("unmute AN1/2");
                dev.set_volume(ChannelId::Output(output), 0, 1.0)
                    .expect("set AN1/2 level");
            }
            dev.set_loopback(1, true).expect("enable AN1/2 loopback");
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
            let mut max_inputs = vec![0.0f32; 18];
            while std::time::Instant::now() < deadline {
                let _ = dev.poll_events();
                if let Some(levels) = dev.meters() {
                    for (i, level) in levels.iter().enumerate().take(18) {
                        if *level > max_inputs[i] {
                            max_inputs[i] = *level;
                        }
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            println!("input meter maxima: {:?}", &max_inputs[..8]);
            dev.apply_scene(&before).expect("restore pre-probe scene");
            println!("PASS loopback probe");
        }
        "meters" => {
            use tuxmix_core::pw_meter::PlaybackTap;
            let tap = PlaybackTap::start();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
            let mut max_input = vec![0.0f32; 18];
            let mut max_playback = vec![0.0f32; 18];
            let mut max_output = vec![0.0f32; 18];
            let mut max_tap = vec![0.0f32; 18];
            let update = |dst: &mut [f32], src: &[f32]| {
                for (d, s) in dst.iter_mut().zip(src.iter()) {
                    if *s > *d {
                        *d = *s;
                    }
                }
            };
            while std::time::Instant::now() < deadline {
                let _ = dev.poll_events();
                if let Some(v) = dev.meters() {
                    update(&mut max_input, &v);
                }
                if let Some(v) = dev.playback_meters() {
                    update(&mut max_playback, &v);
                }
                if let Some(v) = dev.output_meters(true) {
                    update(&mut max_output, &v);
                }
                let (tap_levels, _channels) = tap.drain();
                update(&mut max_tap, &tap_levels);
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            println!("input   {:?}", &max_input[..8]);
            println!("playback {:?}", &max_playback[..8]);
            println!("output  {:?}", &max_output[..8]);
            println!("tap     {:?}", &max_tap[..8]);
            println!("PASS meters");
        }
        other => panic!("unknown command {other}"),
    }
}

#[cfg(not(feature = "alsa"))]
fn main() {
    eprintln!("build with --features alsa");
}
