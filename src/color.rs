#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

pub const DEFAULT_FG: Rgb = Rgb(0xd0, 0xd0, 0xd0);
pub const DEFAULT_BG: Rgb = Rgb(0x10, 0x10, 0x14);

const BASE_PALETTE: [Rgb; 8] = [
    Rgb(0x00, 0x00, 0x00),
    Rgb(0xcd, 0x31, 0x31),
    Rgb(0x0d, 0xbc, 0x79),
    Rgb(0xe5, 0xe5, 0x10),
    Rgb(0x24, 0x72, 0xc8),
    Rgb(0xbc, 0x3f, 0xbc),
    Rgb(0x11, 0xa8, 0xcd),
    Rgb(0xe5, 0xe5, 0xe5),
];

const BRIGHT_PALETTE: [Rgb; 8] = [
    Rgb(0x66, 0x66, 0x66),
    Rgb(0xf1, 0x4c, 0x4c),
    Rgb(0x23, 0xd1, 0x8b),
    Rgb(0xf5, 0xf5, 0x43),
    Rgb(0x3b, 0x8e, 0xea),
    Rgb(0xd6, 0x70, 0xd6),
    Rgb(0x29, 0xb8, 0xdb),
    Rgb(0xff, 0xff, 0xff),
];

/// SGR 30-37 / 90-97 / 40-47 / 100-107.
pub fn ansi_basic(n: u8, bright: bool) -> Rgb {
    let table = if bright { &BRIGHT_PALETTE } else { &BASE_PALETTE };
    table[(n & 7) as usize]
}

/// SGR 38;5;n / 48;5;n. Colors 0-15 share the basic palette; 16-231 are a
/// 6×6×6 cube; 232-255 are 24 grays.
pub fn ansi_256(n: u8) -> Rgb {
    if n < 16 {
        ansi_basic(n & 7, n >= 8)
    } else if n < 232 {
        let i = n - 16;
        let r = i / 36;
        let g = (i % 36) / 6;
        let b = i % 6;
        let scale = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
        Rgb(scale(r), scale(g), scale(b))
    } else {
        let v = 8 + (n - 232) * 10;
        Rgb(v, v, v)
    }
}
