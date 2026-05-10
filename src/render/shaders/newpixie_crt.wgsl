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

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let warped = curve(in.uv);

    // Outside the (warped) screen rect: pure black bezel.
    if (warped.x < 0.0 || warped.x > 1.0 || warped.y < 0.0 || warped.y > 1.0) {
        return vec4<f32>(0.0, 0.0, 0.0, 1.0);
    }

    var color = textureSample(src_tex, src_sampler, warped).rgb;

    let dim = vec2<f32>(textureDimensions(src_tex, 0));

    // Scanlines: every other physical row dimmed.
    let line_phase = fract(warped.y * dim.y * 0.5);
    let scan = mix(0.85, 1.0, smoothstep(0.0, 0.5, abs(line_phase - 0.5) * 2.0));
    color *= scan;

    // RGB phosphor mask: stripe of slightly tinted columns.
    let col = i32(floor(warped.x * dim.x)) % 3;
    var mask = vec3<f32>(1.0, 1.0, 1.0);
    if (col == 0) { mask = vec3<f32>(1.05, 0.92, 0.92); }
    else if (col == 1) { mask = vec3<f32>(0.92, 1.05, 0.92); }
    else { mask = vec3<f32>(0.92, 0.92, 1.05); }
    color *= mask;

    // Vignette: dim the corners.
    let centered = warped - vec2<f32>(0.5, 0.5);
    let v = clamp(1.0 - dot(centered * 1.4, centered * 1.4), 0.55, 1.0);
    color *= v;

    // Ambient phosphor glow — additive so the scanline + RGB-mask pattern
    // remains visible on near-black cells. Kept achromatic so dark areas
    // don't pick up an unintended color cast; the per-column RGB mask still
    // produces the subtle striping.
    let ambient = 0.012;
    color += vec3<f32>(ambient) * mask * scan * v;

    // The offscreen texture is sRGB so `textureSample` returned linear values
    // and wgpu will gamma-encode our output back to sRGB on write — keep the
    // result in linear space here, no extra gamma correction.
    return vec4<f32>(color, 1.0);
}
