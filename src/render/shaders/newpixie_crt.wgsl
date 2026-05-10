// newpixie-crt — adapted from the libretro newpixie-crt slang shader by
// Mattias Gustavsson (MIT / public domain). Single-pass approximation of
// the original slang chain: same curve, tsample stretch, RGB chromatic
// split, polynomial color curve, scanline + 3px shadow mask, and the
// classic 16xy(1-x)(1-y) vignette. The original's separate accumulator
// (ghosting) and Gaussian blur passes are stood in for by a 13-tap
// in-shader bloom; animation parts (rolling scanlines, noise, flicker)
// are dropped because they need a per-frame uniform we don't push yet.
//
// The output surface is sRGB so wgpu re-encodes our linear-space output
// on write — this is why we use a Reinhard-style linear roll-off at the
// end instead of the original's Hable filmic, which has a gamma curve
// baked in and would double-encode here.

@group(0) @binding(0) var src_tex: texture_2d<f32>;
@group(0) @binding(1) var src_sampler: sampler;
// Theme background as a linear-space color (alpha unused). Set once at
// startup; the corner fade lerps toward this so the vignette settles
// into the configured theme bg rather than pure black.
@group(0) @binding(2) var<uniform> theme_bg: vec4<f32>;
// Previous frame's CRT output, for phosphor persistence. The Rust side
// ping-pongs between two textures so the slot we read here is never
// the slot we write to in this pass.
@group(0) @binding(3) var history_tex: texture_2d<f32>;

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

// Full-screen triangle that covers [-1, 1] x [-1, 1] in clip space; uv
// runs 0..1 across the visible region thanks to the over-sized triangle.
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

// Tunables that shape the CRT feel.
const CURVATURE:       f32 = 2.0;  // newpixie default
// Pixelation: snap source-sample UV to a coarse logical-pixel grid so
// glyphs read as chunky CRT pixels. Size in physical pixels — 1.0 off,
// 2-3 typical.
const PIXEL_SIZE:      f32 = 2.0;
// 3-pixel shadow mask, matching the original `1 - 0.23 * mod(x,3)/2`.
const MASK_STRENGTH:   f32 = 0.23;
// Per-channel chromatic offset in *physical pixels*. The slang uses
// uv-space (+0.0009, -0.0011, -0.0015); pixel-units stays consistent
// across resolutions and survives PIXEL_SIZE quantization. Horizontal
// only — vertical splits make the visible centroid of multi-channel
// colors land at different y depending on which channels are present
// (magenta = R+B with no G floats up, green sits low, etc.), which
// reads as the text drifting up/down by color.
const SPLIT_R_PX: vec2<f32> = vec2<f32>( 1.6, 0.0);
const SPLIT_G_PX: vec2<f32> = vec2<f32>( 0.0, 0.0);
const SPLIT_B_PX: vec2<f32> = vec2<f32>(-2.8, 0.0);
// Bloom: extract pixels brighter than THRESHOLD, blur via 13-tap
// weighted Gaussian-ish kernel, add as a phosphor halo. Stands in for
// the original's separate horizontal+vertical Gaussian blur passes.
const BLOOM_THRESHOLD: f32 = 0.35;
const BLOOM_INTENSITY: f32 = 0.55;
const BLOOM_RADIUS_PX: f32 = 4.0;
// Vignette: `16xy(1-x)(1-y)` peaks at 1.0 in the center, 0 at edges.
// Multiplied by VIG_GAIN for the lit-phosphor punch in the screen body.
const VIG_GAIN:  f32 = 1.55;
// Additive center glow weighted by the same vignette shape. Multiplying
// by VIG_GAIN only scales what's already lit, so dark background pixels
// stay dark even at the center. CENTER_LIFT adds a flat amount on top,
// so the whole middle of the screen reads as gently emissive — closer
// to the ambient phosphor glow of a real CRT.
const CENTER_LIFT: f32 = 0.04;
// Shape of the vignette mix. The slang shader uses sqrt (≈0.5) which
// keeps the corners pretty lit; raising the exponent pulls the falloff
// inward so the four-corner gradient toward theme_bg reads more clearly
// without darkening the body. 1.0 ≈ linear; >1.0 sharpens further.
const VIG_FALLOFF: f32 = 0.85;
// Ambient phosphor lift on the inside of the curved screen — the slang
// uses +0.02 per channel, but in our linear-space pipeline that gets
// gamma-expanded to a visible gray. Keep it tiny.
const AMBIENT:   f32 = 0.005;
// Phosphor persistence (afterglow). Two independent knobs:
//   DECAY   — per-frame multiplier on the stored trail. Controls *how
//             long* afterglow lingers. Half-life ≈ -log(2)/log(decay):
//             0.25 → 0.50f, 0.35 → 0.66f, 0.50 → 1.0f, 0.65 → 1.6f.
//   STRENGTH — how much of the decayed trail is mixed into the visible
//             image. Controls *how bright* the afterglow looks. The
//             stored history is unaffected by this — only the surface
//             output is dimmed, so the trail still fades on the same
//             schedule but reads as a softer ghost. 0.0 disables.
const PHOSPHOR_DECAY:    f32 = 0.25;
const PHOSPHOR_STRENGTH: f32 = 0.4;

// Original newpixie barrel curvature. Per-axis stretch, then a
// quadratic widening proportional to the orthogonal coordinate, then
// the inset `*0.92 + 0.04` that places the visible CRT inside a small
// bezel. Returns uv that may extend outside [0,1] in the corners.
fn curve(uv: vec2<f32>) -> vec2<f32> {
    var u = uv - vec2<f32>(0.5);
    u *= vec2<f32>(0.925, 1.095);
    u *= CURVATURE;
    u.x *= 1.0 + pow(abs(u.y) / 4.0, 2.0);
    u.y *= 1.0 + pow(abs(u.x) / 3.0, 2.0);
    u /= CURVATURE;
    u += vec2<f32>(0.5);
    return u * 0.92 + vec2<f32>(0.04);
}

// `tsample` from the original: a slight stretch + offset baked into
// every sample, plus a 1.25x brightness boost. The `pow(2.2)` gamma
// decode is a no-op for us because wgpu already returns linear values
// from the sRGB source texture.
fn tsample(tc: vec2<f32>, dim: vec2<f32>) -> vec3<f32> {
    var c = tc * vec2<f32>(1.025, 0.92) + vec2<f32>(-0.0125, 0.04);
    c = clamp(quantize(c, dim), vec2<f32>(0.0), vec2<f32>(1.0));
    return textureSample(src_tex, src_sampler, c).rgb * 1.25;
}

// Snap UV to the logical-pixel grid set by PIXEL_SIZE.
fn quantize(uv: vec2<f32>, dim: vec2<f32>) -> vec2<f32> {
    let g = dim / PIXEL_SIZE;
    return (floor(uv * g) + 0.5) / g;
}

// Brightness extractor — keep only the part of `c` above
// BLOOM_THRESHOLD, preserving hue.
fn bright(c: vec3<f32>) -> vec3<f32> {
    let lum = dot(c, vec3<f32>(0.299, 0.587, 0.114));
    let factor = max(lum - BLOOM_THRESHOLD, 0.0) / max(lum, 1e-6);
    return c * factor;
}

// 13-tap weighted bloom. Center + 8 inner-ring + 4 outer cardinals.
// Each tap goes through `bright` so the result is just the spread glow.
fn bloom(uv: vec2<f32>, dim: vec2<f32>) -> vec3<f32> {
    let t = BLOOM_RADIUS_PX / dim;
    var s = bright(textureSample(src_tex, src_sampler, uv).rgb) * 1.00;
    s = s + bright(textureSample(src_tex, src_sampler, uv + vec2<f32>( 1.0,  0.0) * t).rgb) * 0.70;
    s = s + bright(textureSample(src_tex, src_sampler, uv + vec2<f32>(-1.0,  0.0) * t).rgb) * 0.70;
    s = s + bright(textureSample(src_tex, src_sampler, uv + vec2<f32>( 0.0,  1.0) * t).rgb) * 0.70;
    s = s + bright(textureSample(src_tex, src_sampler, uv + vec2<f32>( 0.0, -1.0) * t).rgb) * 0.70;
    s = s + bright(textureSample(src_tex, src_sampler, uv + vec2<f32>( 0.7,  0.7) * t).rgb) * 0.50;
    s = s + bright(textureSample(src_tex, src_sampler, uv + vec2<f32>(-0.7,  0.7) * t).rgb) * 0.50;
    s = s + bright(textureSample(src_tex, src_sampler, uv + vec2<f32>( 0.7, -0.7) * t).rgb) * 0.50;
    s = s + bright(textureSample(src_tex, src_sampler, uv + vec2<f32>(-0.7, -0.7) * t).rgb) * 0.50;
    s = s + bright(textureSample(src_tex, src_sampler, uv + vec2<f32>( 2.0,  0.0) * t).rgb) * 0.30;
    s = s + bright(textureSample(src_tex, src_sampler, uv + vec2<f32>(-2.0,  0.0) * t).rgb) * 0.30;
    s = s + bright(textureSample(src_tex, src_sampler, uv + vec2<f32>( 0.0,  2.0) * t).rgb) * 0.30;
    s = s + bright(textureSample(src_tex, src_sampler, uv + vec2<f32>( 0.0, -2.0) * t).rgb) * 0.30;
    return s / 7.6;
}

// Reinhard-style highlight roll-off in linear space. The original's
// Hable filmic doubles as a gamma encode and would double-encode on our
// sRGB surface. This curve only bends above-1 values back into range.
fn rolloff(c: vec3<f32>) -> vec3<f32> {
    return c / (1.0 + max(c - vec3<f32>(1.0), vec3<f32>(0.0)));
}

struct FsOut {
    // Surface — what the user sees this frame.
    @location(0) surface: vec4<f32>,
    // History — same value, fed back next frame as `history_tex` so the
    // afterglow can decay over time. Both attachments share format.
    @location(1) history: vec4<f32>,
};

@fragment
fn fs_main(in: VsOut) -> FsOut {
    let dim = vec2<f32>(textureDimensions(src_tex, 0));

    // Original's blend of curve and identity, then a ~10% expand so
    // content fills slightly past the curved boundary, plus a tiny
    // alignment nudge — the same recipe the slang shader uses.
    let curved_uv = mix(curve(in.uv), in.uv, 0.4);
    let scale = -0.101;
    let scuv = curved_uv * (1.0 - scale)
        + vec2<f32>(scale * 0.5)
        + vec2<f32>(0.003, -0.001);

    let texel = vec2<f32>(1.0) / dim;

    // RGB chromatic split via tsample. Per-channel offsets in pixel
    // units; tsample applies the stretch + brightness boost + quantize.
    var col = vec3<f32>(0.0);
    col.r = tsample(scuv + SPLIT_R_PX * texel, dim).r + AMBIENT;
    col.g = tsample(scuv + SPLIT_G_PX * texel, dim).g + AMBIENT;
    col.b = tsample(scuv + SPLIT_B_PX * texel, dim).b + AMBIENT;

    // Bloom — soft phosphor halo standing in for the original's blur
    // pass. Added before the polynomial so the halo gets the same
    // brightness treatment as direct samples.
    col = col + BLOOM_INTENSITY * bloom(clamp(scuv, vec2<f32>(0.0), vec2<f32>(1.0)), dim);

    // Saturation bias — original uses (0.95, 1.05, 0.95) for the green
    // phosphor cast. Faithful match here.
    col *= vec3<f32>(0.95, 1.05, 0.95);

    // Polynomial color curve, ported verbatim. Lifts midtones and
    // compresses to give the punchy phosphor response.
    col = clamp(
        col * 1.3 + 0.75 * col * col + 1.25 * col * col * col * col * col,
        vec3<f32>(0.0),
        vec3<f32>(10.0),
    );

    // Vignette on `curved_uv`, matching the original. Brightens the body
    // by ~1.3x and falls off to 0 at the edges. We then lerp toward
    // theme_bg using the same shape so the corners settle at the
    // configured background instead of black.
    let v = 16.0 * curved_uv.x * curved_uv.y
        * (1.0 - curved_uv.x) * (1.0 - curved_uv.y);
    let vig_alpha = pow(max(v, 0.0), VIG_FALLOFF);
    let lit = col * VIG_GAIN + vec3<f32>(CENTER_LIFT) * vig_alpha;
    col = mix(theme_bg.rgb, lit, vig_alpha);

    // Static-Y scanlines, no time animation: same `0.35 + 0.18*sin`
    // shape as the original, then `pow(s, 0.9)`. Multiplies the body.
    let scans = clamp(0.35 + 0.18 * sin(-in.uv.y * dim.y * 1.5), 0.0, 1.0);
    col *= pow(scans, 0.9);

    // 3-pixel vertical shadow mask in framebuffer space.
    let mask_phase = clamp((in.clip.x - floor(in.clip.x / 3.0) * 3.0) / 2.0, 0.0, 1.0);
    col *= 1.0 - MASK_STRENGTH * mask_phase;

    let final_col = rolloff(col);

    // Phosphor persistence. Sample the previous frame at the same screen
    // position (pre-curve uv — the history is post-CRT, already curved)
    // and max-blend with the decayed previous value. Max keeps the
    // current frame fully bright while letting prior bright pixels
    // linger as a trail.
    //
    // The *stored* history uses the full decayed trail so the fade
    // schedule is independent of how visible we make it; the *surface*
    // output mixes between the live frame and that trail by STRENGTH,
    // so a lower STRENGTH dims the ghost without making it shorter.
    let prev = textureSample(history_tex, src_sampler, in.uv).rgb;
    let trail = max(final_col, prev * PHOSPHOR_DECAY);
    let visible = mix(final_col, trail, PHOSPHOR_STRENGTH);

    var out: FsOut;
    out.surface = vec4<f32>(visible, 1.0);
    out.history = vec4<f32>(trail, 1.0);
    return out;
}
