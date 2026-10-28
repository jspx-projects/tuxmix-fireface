//! Temporary UC matrix probe: raw SW1/2 -> AN3/4, plus masters.
use tuxmix_core::fireface::{Direction, Hwdep};
use tuxmix_core::fireface::protocol;
use tuxmix_core::fireface::protocol::uc;
use tuxmix_core::{ChannelId, Fireface, RmeDevice};

fn main() {
    let on = std::env::args().nth(1).as_deref() != Some("off");
    let v = if on { 1.0 } else { 0.0 };
    let hw = Hwdep::open(2).expect("hwdep");
    // Verified on the UC bench at 128/44.1: SW1/2 are matrix sources 18/19.
    for (out, src) in [(2u16, 18u16), (3u16, 19u16)] {
        let (value, index) = uc::matrix_word(v, out * uc::MIX_STRIDE + src);
        hw.vendor(protocol::req::MIXER, Direction::Out, value, index, &[])
            .expect("set matrix");
    }
    let mut dev = Fireface::open_uc().expect("UC hwdep");
    for idx in [4usize, 5] {
        dev.set_volume(ChannelId::Output(idx), idx, v).expect("output volume");
    }
    hw.vendor(protocol::req::UNMUTE, Direction::Out, 0xffff, 0xc000, &[])
        .expect("global unmute");
    for out in [2u16, 3u16] {
        let (value, index) = uc::matrix_word(v, uc::master_address(out as usize));
        hw.vendor(protocol::req::MIXER, Direction::Out, value, index, &[])
            .expect("master");
    }
    println!("raw SW1/2(20/21) -> AN3/4, masters {}", if on { "0 dB" } else { "-inf" });
}
