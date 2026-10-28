//! Temporary probe: route one source pair to every matrix output and set
//! every output master/attenuator to 0 dB.  Usage: uc-broadcast <source>

use tuxmix_core::fireface::protocol;
use tuxmix_core::fireface::protocol::uc;
use tuxmix_core::fireface::{Direction, Hwdep};
use tuxmix_core::{ChannelId, Fireface, RmeDevice};

fn main() {
    let source = std::env::args()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(20);
    let hw = Hwdep::open(2).expect("hwdep");
    let mut dev = Fireface::open_uc().expect("UC hwdep");

    for out in 0..18u16 {
        for src in 0..uc::MIX_STRIDE {
            let (value, index) = uc::matrix_word(0.0, out * uc::MIX_STRIDE + src);
            hw.vendor(protocol::req::MIXER, Direction::Out, value, index, &[])
                .expect("clear matrix");
        }
    }
    for out in 0..18u16 {
        for side in 0..2u16 {
            let (value, index) =
                uc::matrix_word(1.0, out * uc::MIX_STRIDE + source + side);
            hw.vendor(protocol::req::MIXER, Direction::Out, value, index, &[])
                .expect("matrix");
            let (value, index) = uc::matrix_word(1.0, uc::master_address(out as usize));
            hw.vendor(protocol::req::MIXER, Direction::Out, value, index, &[])
                .expect("master");
        }
    }
    for idx in 0..18usize {
        dev.set_volume(ChannelId::Output(idx), idx, 1.0)
            .expect("output volume");
    }
    for value in [0xffffu16, 0x000fu16] {
        hw.vendor(protocol::req::UNMUTE, Direction::Out, value, 0xc000, &[])
            .expect("unmute");
    }
    println!("source {}/{} routed to all 18 outputs at 0 dB", source, source + 1);
}
