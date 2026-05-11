//! Built-in post-processing shaders. Compiled into the binary so they
//! resolve without filesystem IO. Anything not listed here falls back to
//! a user shader under `~/.config/evelyn/shaders/`.

pub fn builtin_shader_source(name: &str) -> Option<&'static str> {
    match name {
        "newpixie-crt" => Some(include_str!("newpixie_crt.wgsl")),
        _ => None,
    }
}
