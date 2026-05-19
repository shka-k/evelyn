use crate::color::Rgb;

pub(super) fn clear_color_for_with_alpha(bg: Rgb, a: f32) -> wgpu::Color {
    wgpu::Color {
        r: srgb_to_linear(bg.0),
        g: srgb_to_linear(bg.1),
        b: srgb_to_linear(bg.2),
        a: a.clamp(0.0, 1.0) as f64,
    }
}

/// Pre-linearise sRGB color values for the quad pipeline. The wgpu surface
/// format is sRGB, so the fragment shader output is treated as linear and
/// gamma-encoded on write. Without this conversion every solid quad would
/// render visibly lighter than the source color.
pub(super) fn rgb_to_rgba(c: Rgb, a: f32) -> [f32; 4] {
    [
        srgb_to_linear(c.0) as f32,
        srgb_to_linear(c.1) as f32,
        srgb_to_linear(c.2) as f32,
        a,
    ]
}

pub(super) fn srgb_to_linear(c: u8) -> f64 {
    let s = c as f64 / 255.0;
    if s <= 0.04045 {
        s / 12.92
    } else {
        ((s + 0.055) / 1.055).powf(2.4)
    }
}
