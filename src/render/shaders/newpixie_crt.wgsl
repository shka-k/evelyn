// newpixie-crt — adapted from the libretro newpixie-crt slang shader by
// Mattias Gustavsson (MIT / public domain). Light barrel curvature, RGB
// chromatic split, soft bloom, scanlines, 3-pixel shadow mask, and the
// original "product of distances" vignette. Single-pass: bloom is faked
// with a 13-tap weighted blur in the same fragment shader. Surface
// format is sRGB so the fragment output stays in linear space and wgpu
// encodes on write.

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
const SCANLINE_PERIOD: f32 = 4.0;  // physical rows per scanline cycle
const SCANLINE_DEPTH:  f32 = 0.75; // dimmest point of a scanline (0..1)
// 3-pixel shadow mask, ported from the original. `MASK_STRENGTH` is the
// peak dim factor on the darkest column of the cycle.
const MASK_STRENGTH:   f32 = 0.18;
// Per-channel uv offsets — the slang shader uses (+0.0009, +0.0009),
// (0, -0.0011), (-0.0015, 0); same idea here, scaled a hair smaller.
const SPLIT_R: vec2<f32> = vec2<f32>( 0.0008,  0.0008);
const SPLIT_G: vec2<f32> = vec2<f32>( 0.0000, -0.0010);
const SPLIT_B: vec2<f32> = vec2<f32>(-0.0014,  0.0000);
// Bloom: extract pixels brighter than THRESHOLD, blur via 13-tap weighted
// Gaussian-ish kernel, add back as a phosphor halo. RADIUS_PX is the
// outer-ring tap distance in source pixels — larger spreads further but
// quality falls off past ~6 since we only have 13 taps to cover it.
const BLOOM_THRESHOLD: f32 = 0.35;
const BLOOM_INTENSITY: f32 = 0.75;
const BLOOM_RADIUS_PX: f32 = 4.0;
// Newpixie vignette: `16 * x * y * (1-x) * (1-y)` peaks at 1.0 in the
// center and is 0 along every edge. Multiplied by VIG_GAIN to slightly
// overshoot 1.0 in the body for the lit-phosphor look; VIG_FLOOR is
// added inside the sqrt so corners settle at a soft dim instead of
// crushing to true black.
const VIG_GAIN:  f32 = 1.55;
const VIG_FLOOR: f32 = 0.04;

// RGB chromatic split sample. Pulls each channel from a slightly offset
// uv so bright glyphs get a faint phosphor color fringe.
fn split_sample(uv: vec2<f32>) -> vec3<f32> {
    let r = textureSample(src_tex, src_sampler, clamp(uv + SPLIT_R, vec2<f32>(0.0), vec2<f32>(1.0))).r;
    let g = textureSample(src_tex, src_sampler, clamp(uv + SPLIT_G, vec2<f32>(0.0), vec2<f32>(1.0))).g;
    let b = textureSample(src_tex, src_sampler, clamp(uv + SPLIT_B, vec2<f32>(0.0), vec2<f32>(1.0))).b;
    return vec3<f32>(r, g, b);
}

// Brightness extractor — return the part of `c` above BLOOM_THRESHOLD,
// preserving hue (scale by `excess / lum`).
fn bright(c: vec3<f32>) -> vec3<f32> {
    let lum = dot(c, vec3<f32>(0.299, 0.587, 0.114));
    let factor = max(lum - BLOOM_THRESHOLD, 0.0) / max(lum, 1e-6);
    return c * factor;
}

// 13-tap weighted bloom: center + 8 inner-ring taps + 4 outer cardinals.
// Each tap goes through `bright` so the result is just the spread glow,
// suitable for additive composition over the body color.
fn bloom(uv: vec2<f32>, dim: vec2<f32>) -> vec3<f32> {
    let t = BLOOM_RADIUS_PX / dim;
    var s = bright(textureSample(src_tex, src_sampler, uv).rgb) * 1.00;
    // Inner ring (radius 1·t) — strong contribution.
    s = s + bright(textureSample(src_tex, src_sampler, uv + vec2<f32>( 1.0,  0.0) * t).rgb) * 0.70;
    s = s + bright(textureSample(src_tex, src_sampler, uv + vec2<f32>(-1.0,  0.0) * t).rgb) * 0.70;
    s = s + bright(textureSample(src_tex, src_sampler, uv + vec2<f32>( 0.0,  1.0) * t).rgb) * 0.70;
    s = s + bright(textureSample(src_tex, src_sampler, uv + vec2<f32>( 0.0, -1.0) * t).rgb) * 0.70;
    s = s + bright(textureSample(src_tex, src_sampler, uv + vec2<f32>( 0.7,  0.7) * t).rgb) * 0.50;
    s = s + bright(textureSample(src_tex, src_sampler, uv + vec2<f32>(-0.7,  0.7) * t).rgb) * 0.50;
    s = s + bright(textureSample(src_tex, src_sampler, uv + vec2<f32>( 0.7, -0.7) * t).rgb) * 0.50;
    s = s + bright(textureSample(src_tex, src_sampler, uv + vec2<f32>(-0.7, -0.7) * t).rgb) * 0.50;
    // Outer cardinals (radius 2·t) — faint long-tail glow.
    s = s + bright(textureSample(src_tex, src_sampler, uv + vec2<f32>( 2.0,  0.0) * t).rgb) * 0.30;
    s = s + bright(textureSample(src_tex, src_sampler, uv + vec2<f32>(-2.0,  0.0) * t).rgb) * 0.30;
    s = s + bright(textureSample(src_tex, src_sampler, uv + vec2<f32>( 0.0,  2.0) * t).rgb) * 0.30;
    s = s + bright(textureSample(src_tex, src_sampler, uv + vec2<f32>( 0.0, -2.0) * t).rgb) * 0.30;
    return s / 7.6; // sum of weights
}

// Reinhard-style highlight roll-off. Pure linear-space operator — no
// gamma baked in (unlike the Hable filmic in the original, which assumes
// the framebuffer is non-sRGB). Our surface is sRGB so wgpu re-encodes
// on write; mixing in a gamma'd tone-map here produced washed-out
// midtones. This curve only bends values above ~1.0 back into range.
fn rolloff(c: vec3<f32>) -> vec3<f32> {
    return c / (1.0 + max(c - vec3<f32>(1.0), vec3<f32>(0.0)));
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let warped = curve(in.uv);

    // Clamp-sample at the curved screen edge — vignette below crushes
    // those pixels to near-black anyway, so the bezel reads as a smooth
    // gradient instead of a hard cutoff.
    let sample_uv = clamp(warped, vec2<f32>(0.0), vec2<f32>(1.0));
    let dim = vec2<f32>(textureDimensions(src_tex, 0));

    // Per-channel chromatic split. The original lifts every pixel by
    // +0.02 here, but that constant sits in linear space and gets
    // gamma-expanded to a visible gray on our sRGB surface — which
    // floated all the blacks. Skip it; bloom + ambient below still
    // give phosphor warmth on lit cells.
    var color = split_sample(sample_uv);

    // Soft saturation bias — original uses (0.95, 1.05, 0.95) to nudge
    // greens up; gentler here so the theme palette stays recognizable.
    color *= vec3<f32>(0.99, 1.02, 0.99);

    // Bloom: blurred bright pixels added on top, before scan/mask so the
    // halo crosses scanlines like real phosphor glow.
    color = color + BLOOM_INTENSITY * bloom(sample_uv, dim);

    // Thick scanlines spanning multiple physical rows.
    let line_phase = fract(warped.y * dim.y / SCANLINE_PERIOD);
    let scan = mix(SCANLINE_DEPTH, 1.0, smoothstep(0.0, 0.5, abs(line_phase - 0.5) * 2.0));
    color *= scan;

    // 3-pixel shadow mask in framebuffer space — bright/medium/dim cycle.
    // Same shape as the original `1.0 - 0.23 * clamp(mod(x,3)/2, 0, 1)`,
    // with strength factored out so it can be tuned.
    let mask_phase = clamp((in.clip.x - floor(in.clip.x / 3.0) * 3.0) / 2.0, 0.0, 1.0);
    color *= 1.0 - MASK_STRENGTH * mask_phase;

    // Newpixie vignette on the unwarped uv. Smooth product → no seam
    // between the curved body and the corners; sqrt curve keeps the
    // falloff gentle through the body and steeper near the edges.
    let v = 16.0 * in.uv.x * in.uv.y * (1.0 - in.uv.x) * (1.0 - in.uv.y);
    let vig = VIG_GAIN * sqrt(VIG_FLOOR + max(v, 0.0));
    color *= vig;

    // Tiny ambient phosphor floor — kept very small because it lives in
    // linear space, modulated by mask/scan/vig so it doesn't wash the
    // bezel area. Only really visible inside the lit body.
    color += vec3<f32>(0.003) * scan * vig;

    // Roll bright bloom peaks back; leave dark/mid values alone so we
    // don't relift blacks toward gray.
    return vec4<f32>(rolloff(color), 1.0);
}
