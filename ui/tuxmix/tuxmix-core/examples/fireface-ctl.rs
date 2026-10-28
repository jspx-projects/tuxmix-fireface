//! Read and write raw UC control values through the hwdep node.
//!
//!   fireface-ctl <card> info
//!   fireface-ctl <card> read <req>
//!   fireface-ctl <card> raw <req> <value> <index>
//!   fireface-ctl <card> mix <out_block> <source> <linear>
//!   fireface-ctl <card> master <out> <linear>
//!   fireface-ctl <card> output <encoder_channel> <linear>
//!   fireface-ctl <card> gain <input> <db>
//!   fireface-ctl <card> switch <bit> <0|1>
//!   fireface-ctl <card> loopback <index> <0|1>
//!   fireface-ctl <card> word <addr> <value>   (UCX IF1 register)
//!
//! `linear` is 0.0..1.0; 1.0 = 0 dB in the UC matrix scale.

#[cfg(feature = "alsa")]
fn main() {
    use tuxmix_core::fireface::hwdep::Direction;
    use tuxmix_core::fireface::protocol::uc;
    use tuxmix_core::fireface::Hwdep;

    let mut args = std::env::args().skip(1);
    let card: i32 = args
        .next()
        .and_then(|s| s.parse().ok())
        .expect("usage: fireface-ctl <card> <command> ...");
    let hw = Hwdep::open(card).expect("open hwdep");
    let cmd = args.next().unwrap_or_else(|| "info".into());

    let num = |s: Option<String>, name: &str| -> f64 {
        s.and_then(|v| v.parse().ok())
            .unwrap_or_else(|| panic!("missing/invalid {name}"))
    };
    let int = |s: Option<String>, name: &str| -> u64 {
        let v = s.unwrap_or_else(|| panic!("missing {name}"));
        if let Some(h) = v.strip_prefix("0x").or_else(|| v.strip_prefix("0X")) {
            u64::from_str_radix(h, 16).unwrap_or_else(|_| panic!("invalid {name}"))
        } else {
            v.parse().unwrap_or_else(|_| panic!("invalid {name}"))
        }
    };

    match cmd.as_str() {
        "info" => {
            let i = hw.info().expect("info");
            println!(
                "card {}: {} ({}) abi={} model={} transport={} rate={} channels={} meter={}B",
                card,
                i.name_str(),
                i.serial_str(),
                i.abi,
                i.model,
                i.transport,
                i.rate,
                i.channels,
                i.meter_bytes
            );
        }
        "read" => {
            let req = int(args.next(), "req") as u8;
            let value = args
                .next()
                .map(|v| int(Some(v), "value") as u16)
                .unwrap_or(0);
            let index = args
                .next()
                .map(|v| int(Some(v), "index") as u16)
                .unwrap_or(0);
            let d = hw
                .vendor(req, Direction::In, value, index, &[0u8; 4])
                .expect("vendor read");
            println!("{}", d.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" "));
        }
        "raw" => {
            let req = int(args.next(), "req") as u8;
            let value = int(args.next(), "value") as u16;
            let index = int(args.next(), "index") as u16;
            hw.vendor(req, Direction::Out, value, index, &[])
                .expect("vendor write");
            println!("ok");
        }
        "mix" => {
            let out = int(args.next(), "out_block") as u16;
            let source = int(args.next(), "source") as u16;
            let linear = num(args.next(), "linear") as f32;
            let (value, index) = uc::matrix_word(linear, out * uc::MIX_STRIDE + source);
            hw.vendor(0x12, Direction::Out, value, index, &[])
                .expect("matrix write");
            println!("mix out={out} source={source} linear={linear} -> value=0x{value:04x} index=0x{index:04x}");
        }
        "master" => {
            let out = int(args.next(), "out") as u16;
            let linear = num(args.next(), "linear") as f32;
            let (value, index) = uc::matrix_word(linear, uc::MIX_MASTER + out);
            hw.vendor(0x12, Direction::Out, value, index, &[])
                .expect("master write");
            println!("master out={out} linear={linear} -> value=0x{value:04x} index=0x{index:04x}");
        }
        "output" => {
            let channel = int(args.next(), "encoder_channel") as u16;
            let linear = num(args.next(), "linear") as f32;
            let raw = uc::output_attenuation(linear);
            hw.vendor(0x1a, Direction::Out, raw as u16, channel, &[])
                .expect("output volume write");
            println!("output channel={channel} linear={linear} -> raw={raw}");
        }
        "gain" => {
            let input = int(args.next(), "input") as u16;
            let db = int(args.next(), "db") as u16;
            hw.vendor(0x1a, Direction::Out, db, input, &[])
                .expect("gain write");
            println!("gain input={input} db={db}");
        }
        "switch" => {
            let bit = int(args.next(), "bit") as u16;
            let on = num(args.next(), "on") != 0.0;
            let d = hw
                .vendor(0x17, Direction::In, 0, 0, &[0u8; 4])
                .expect("switch read");
            let current = u32::from_le_bytes([d[0], d[1], d[2], d[3]]);
            let mask = 1u32 << bit;
            let next = if on { current | mask } else { current & !mask };
            hw.vendor(0x17, Direction::Out, next as u16, 0xffff, &[])
                .expect("switch write");
            let d = hw
                .vendor(0x17, Direction::In, 0, 0, &[0u8; 4])
                .expect("switch readback");
            let readback = u32::from_le_bytes([d[0], d[1], d[2], d[3]]);
            println!(
                "switch bit={bit} on={on} 0x17=0x{readback:08x} bit={}",
                (readback & mask != 0) as u8
            );
        }
        "loopback" => {
            let index = int(args.next(), "index") as u16;
            let on = num(args.next(), "on") != 0.0;
            hw.vendor(0x15, Direction::Out, on as u16, index, &[])
                .expect("loopback write");
            println!("loopback index={index} on={on}");
        }
        "word" => {
            let addr = int(args.next(), "addr") as u16;
            let value = int(args.next(), "value") as u16;
            hw.write_words(&[(addr, value)])
                .expect("IF1 word write");
            println!("word addr=0x{addr:04x} value=0x{value:04x}");
        }
        _ => {
            eprintln!("unknown command {cmd}");
            std::process::exit(2);
        }
    }
}

#[cfg(not(feature = "alsa"))]
fn main() {
    eprintln!("build with --features alsa");
}
