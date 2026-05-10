// newpixie-crt — a simplified port of the libretro CRT shader of the same
// name. Light barrel curvature, scanlines, RGB phosphor mask, and corner
// vignette. Surface format is sRGB so the fragment output is linear, hence
// the gamma encode at the end.

@group(0) @binding(0) var src_tex: texture_2d<f32>;
@group(0) @binding(1) var src_sampler: sampler;

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

// Full-screen triangle that covers [-1, 1] x [-1, 1] in clip space; uv runs
// 0..1 across the visible region thanks to the over-sized triangle trick.
@vertex
fn vs_main(@builtin(vertex_index) vid: u32) -> VsOut {
    var positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>( 3.0, -1.0),
        vec2<f32>(-1.0,  3.0),
    );
    var uvs = array<vec2<f32>, 3>(
        vec2<f32>(0.0, 1.0),
        vec2<f32>(2.0, 1.0),
        vec2<f32>(0.0, -1.0),
    );
    var out: VsOut;
    out.clip = vec4<f32>(positions[vid], 0.0, 1.0);
    out.uv = uvs[vid];
    return out;
}

// Slight barrel distortion. Returns the warped uv.
fn curve(uv: vec2<f32>) -> vec2<f32> {
    let centered = uv - vec2<f32>(0.5, 0.5);
    let dist = centered * centered;
    let curvature = vec2<f32>(6.0, 4.0);
    return uv + centered * dist.yx / curvature;
}

// Tunables that shape the CRT feel.
const SCANLINE_PERIOD: f32 = 4.0; // physical rows per scanline cycle
const MASK_STRIPE:    f32 = 6.0;  // phosphor stripe width in physical px
const SCANLINE_DEPTH: f32 = 0.7; // dimmest point of a scanline (0..1)

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let warped = curve(in.uv);

    // Outside the (warped) screen rect: pure black bezel.
    if (warped.x < 0.0 || warped.x > 1.0 || warped.y < 0.0 || warped.y > 1.0) {
        return vec4<f32>(0.0, 0.0, 0.0, 1.0);
    }

    let dim = vec2<f32>(textureDimensions(src_tex, 0));

    // Sharp sample — no UV quantization, so glyph edges stay smooth.
    var color = textureSample(src_tex, src_sampler, warped).rgb;

    // Thick scanlines spanning multiple physical rows.
    let line_phase = fract(warped.y * dim.y / SCANLINE_PERIOD);
    let scan = mix(SCANLINE_DEPTH, 1.0, smoothstep(0.0, 0.5, abs(line_phase - 0.5) * 2.0));
    color *= scan;

    // RGB phosphor mask: subtle per-column tint. Keep amplitudes small so
    // the dim columns don't show up as visible dark vertical bands.
    let col = i32(floor(warped.x * dim.x / MASK_STRIPE)) % 3;
    var mask = vec3<f32>(1.0, 1.0, 1.0);
    if (col == 0) { mask = vec3<f32>(1.04, 0.97, 0.97); }
    else if (col == 1) { mask = vec3<f32>(0.97, 1.04, 0.97); }
    else { mask = vec3<f32>(0.97, 0.97, 1.04); }
    color *= mask;

    // Vignette: dim the corners.
    let centered = warped - vec2<f32>(0.5, 0.5);
    let v = clamp(1.0 - dot(centered * 1.4, centered * 1.4), 0.55, 1.0);
    color *= v;

    // Ambient phosphor glow — additive so the scanline + RGB-mask pattern
    // remains visible on near-black cells. Kept achromatic so dark areas
    // don't pick up an unintended color cast; the per-column RGB mask still
    // produces the subtle striping.
    let ambient = 0.014;
    color += vec3<f32>(ambient) * mask * scan * v;

    // The offscreen texture is sRGB so `textureSample` returned linear values
    // and wgpu will gamma-encode our output back to sRGB on write — keep the
    // result in linear space here, no extra gamma correction.
    return vec4<f32>(color, 1.0);
}
