//! Temporary probe: clear the whole UC matrix, route one source pair to one
//! raw output pair, set all masters/attenuators to 0 dB and unmute.
//! Usage: uc-iso-route <source> <raw-out>

use tuxmix_core::fireface::protocol;
use tuxmix_core::fireface::protocol::uc;
use tuxmix_core::fireface::{Direction, Hwdep};
use tuxmix_core::{ChannelId, Fireface, RmeDevice};

fn main() {
    let source = std::env::args()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(20);
    let out = std::env::args()
        .nth(2)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(2);
    let hw = Hwdep::open(2).expect("hwdep");
    for o in 0..18u16 {
        for src in 0..uc::MIX_STRIDE {
            let (value, index) = uc::matrix_word(0.0, o * uc::MIX_STRIDE + src);
            hw.vendor(protocol::req::MIXER, Direction::Out, value, index, &[])
                .expect("clear matrix");
        }
    }
    for side in 0..2u16 {
        let (value, index) =
            uc::matrix_word(1.0, out * uc::MIX_STRIDE + source + side);
        hw.vendor(protocol::req::MIXER, Direction::Out, value, index, &[])
            .expect("route");
    }
    for o in 0..18u16 {
        let (value, index) = uc::matrix_word(1.0, uc::master_address(o as usize));
        hw.vendor(protocol::req::MIXER, Direction::Out, value, index, &[])
            .expect("master");
    }
    let mut dev = Fireface::open_uc().expect("UC hwdep");
    for idx in 0..18usize {
        dev.set_volume(ChannelId::Output(idx), idx, 1.0)
            .expect("output volume");
    }
    for value in [0xffffu16, 0x000fu16] {
        hw.vendor(protocol::req::UNMUTE, Direction::Out, value, 0xc000, &[])
            .expect("unmute");
    }
    println!("source {}/{} -> raw out {}/{}; all else cleared", source, source + 1, out, out + 1);
}
