use std::sync::Arc;

use anyhow::Result;
use glyphon::{
    cosmic_text::{fontdb, Fallback, PlatformFallback},
    Buffer, Cache, FontSystem, Metrics, Shaping, SwashCache, TextAtlas, TextRenderer, Viewport,
};
use unicode_script::Script;
use wgpu::{
    CompositeAlphaMode, DeviceDescriptor, Instance, InstanceDescriptor, MultisampleState,
    PowerPreference, PresentMode, RequestAdapterOptions, SurfaceConfiguration, TextureUsages,
};
use winit::window::Window;

use crate::config::config;

use super::font_attrs;

/// Primary monospace font (Latin + Nerd Font icons). Bundled so the terminal
/// looks the same out-of-the-box on any host. CJK glyphs come from the host's
/// system fonts via cosmic-text's per-script fallback.
const FONT_PRIMARY_REGULAR: &[u8] =
    include_bytes!("../../assets/fonts/GeistMonoNerdFontMono-Regular.otf");
const FONT_PRIMARY_BOLD: &[u8] =
    include_bytes!("../../assets/fonts/GeistMonoNerdFontMono-Bold.otf");

pub const BUNDLED_FONT_NAME: &str = "GeistMono NFM";

pub(super) struct GpuInit {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub surface: wgpu::Surface<'static>,
    pub config: SurfaceConfiguration,
    pub format: wgpu::TextureFormat,
}

pub(super) fn init_gpu(window: &Arc<Window>) -> Result<GpuInit> {
    let size = window.inner_size();
    let instance = Instance::new(InstanceDescriptor::new_without_display_handle());
    let surface = instance.create_surface(window.clone())?;
    let adapter = pollster::block_on(instance.request_adapter(&RequestAdapterOptions {
        power_preference: PowerPreference::HighPerformance,
        compatible_surface: Some(&surface),
        force_fallback_adapter: false,
    }))?;
    let (device, queue) =
        pollster::block_on(adapter.request_device(&DeviceDescriptor::default()))?;
    let caps = surface.get_capabilities(&adapter);
    let format = caps
        .formats
        .iter()
        .copied()
        .find(|f| f.is_srgb())
        .unwrap_or(caps.formats[0]);
    let config = SurfaceConfiguration {
        usage: TextureUsages::RENDER_ATTACHMENT,
        format,
        width: size.width.max(1),
        height: size.height.max(1),
        present_mode: PresentMode::Fifo,
        alpha_mode: CompositeAlphaMode::Auto,
        view_formats: vec![],
        desired_maximum_frame_latency: 2,
    };
    surface.configure(&device, &config);
    Ok(GpuInit { device, queue, surface, config, format })
}

pub(super) struct TextInit {
    pub font_system: FontSystem,
    pub swash_cache: SwashCache,
    pub viewport: Viewport,
    pub atlas: TextAtlas,
    pub text_renderer: TextRenderer,
}

pub(super) fn init_text_stack(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    format: wgpu::TextureFormat,
) -> TextInit {
    // Mirror what `FontSystem::new()` does — load system fonts into a fresh
    // db — but install our own `Fallback` so glyphs cosmic-text's built-in
    // macOS table doesn't cover (e.g. Braille for gtop sparklines) get
    // routed to a font that has them. Otherwise the primary Geist Mono +
    // common fallbacks (Menlo / Geneva / Arial Unicode MS) all miss
    // U+2800-U+28FF and the run renders as `.notdef` boxes.
    let mut db = fontdb::Database::new();
    db.load_system_fonts();
    db.load_font_data(FONT_PRIMARY_REGULAR.to_vec());
    db.load_font_data(FONT_PRIMARY_BOLD.to_vec());
    let locale = sys_locale::get_locale().unwrap_or_else(|| "en-US".to_string());
    let font_system =
        FontSystem::new_with_locale_and_db_and_fallback(locale, db, EvelynFallback::new());
    let swash_cache = SwashCache::new();
    let cache = Cache::new(device);
    let viewport = Viewport::new(device, &cache);
    let mut atlas = TextAtlas::new(device, queue, &cache, format);
    let text_renderer = TextRenderer::new(&mut atlas, device, MultisampleState::default(), None);
    TextInit { font_system, swash_cache, viewport, atlas, text_renderer }
}

pub(super) fn metrics_for(scale: f32) -> (f32, f32) {
    let cfg = config();
    let font_size = (cfg.font.size_pt * scale).round();
    let line_height = (font_size * cfg.font.line_height_factor).round();
    (font_size, line_height)
}

pub(super) fn make_buffer(font_system: &mut FontSystem, font_size: f32, line_height: f32) -> Buffer {
    Buffer::new(font_system, Metrics::new(font_size, line_height))
}

/// Estimate the advance width of a single monospace glyph by shaping a probe
/// string. Falls back to a fraction of the font size when shaping is empty.
pub(super) fn measure_cell_width(fs: &mut FontSystem, font_size: f32, line_height: f32) -> f32 {
    const PROBE: &str = "MMMMMMMMMM";
    let mut buf = Buffer::new(fs, Metrics::new(font_size, line_height));
    buf.set_size(fs, Some(10_000.0), Some(line_height * 2.0));
    let attrs = font_attrs();
    buf.set_text(fs, PROBE, &attrs, Shaping::Advanced, None);
    buf.shape_until_scroll(fs, false);
    let mut max_x: f32 = 0.0;
    for run in buf.layout_runs() {
        for glyph in run.glyphs.iter() {
            max_x = max_x.max(glyph.x + glyph.w);
        }
    }
    if max_x > 0.0 {
        max_x / PROBE.len() as f32
    } else {
        font_size * 0.6
    }
}

/// Fallback wrapper that augments cosmic-text's `PlatformFallback` with
/// scripts the upstream tables miss. The first hit on macOS is `Braille`:
/// gtop/btop/htop sparklines emit U+2800-U+28FF, the bundled Geist Mono
/// has zero glyphs in that block, and none of cosmic-text's macOS
/// common-fallback fonts (Menlo, Geneva, Arial Unicode MS) cover it
/// either — so without this the whole graph rendered as `.notdef` boxes.
struct EvelynFallback {
    inner: PlatformFallback,
}

impl EvelynFallback {
    fn new() -> Self {
        Self { inner: PlatformFallback }
    }
}

impl Fallback for EvelynFallback {
    fn common_fallback(&self) -> &[&'static str] {
        self.inner.common_fallback()
    }

    fn forbidden_fallback(&self) -> &[&'static str] {
        self.inner.forbidden_fallback()
    }

    fn script_fallback(&self, script: Script, locale: &str) -> &[&'static str] {
        match script {
            Script::Braille => &["Apple Braille"],
            _ => self.inner.script_fallback(script, locale),
        }
    }
}
