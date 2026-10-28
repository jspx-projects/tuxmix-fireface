//! Temporary UC matrix probe: clear AN3/4 and route one stereo playback
//! source pair to it.  Usage: uc-matrix-probe <source> [out]
//! Example: uc-matrix-probe 18 2

use tuxmix_core::fireface::protocol;
use tuxmix_core::fireface::protocol::uc;
use tuxmix_core::fireface::{Direction, Hwdep};
use tuxmix_core::{ChannelId, Fireface, RmeDevice};

fn main() {
    let source = std::env::args()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(18);
    let out = std::env::args()
        .nth(2)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(2);

    let hw = Hwdep::open(2).expect("hwdep");
    // Clear both sides of the AN3/4 output block first, so an old device
    // route cannot masquerade as the result.
    for o in [out, out + 1] {
        for src in 0..uc::MIX_STRIDE {
            let (value, index) = uc::matrix_word(0.0, o * uc::MIX_STRIDE + src);
            hw.vendor(protocol::req::MIXER, Direction::Out, value, index, &[])
                .expect("clear matrix");
        }
    }
    for o in [out, out + 1] {
        let src = source + (o - out);
        let (value, index) = uc::matrix_word(1.0, o * uc::MIX_STRIDE + src);
        hw.vendor(protocol::req::MIXER, Direction::Out, value, index, &[])
            .expect("set matrix");
        let (value, index) = uc::matrix_word(1.0, uc::master_address(o as usize));
        hw.vendor(protocol::req::MIXER, Direction::Out, value, index, &[])
            .expect("set master");
    }

    let mut dev = Fireface::open_uc().expect("UC hwdep");
    for idx in [out as usize * 2, out as usize * 2 + 1] {
        dev.set_volume(ChannelId::Output(idx), idx, 1.0)
            .expect("set output volume");
    }
    hw.vendor(protocol::req::UNMUTE, Direction::Out, 0xffff, 0xc000, &[])
        .expect("global unmute");

    println!("cleared AN3/4, routed source pair {}/{} to raw out {}/{} at 0 dB",
             source, source + 1, out, out + 1);
}
