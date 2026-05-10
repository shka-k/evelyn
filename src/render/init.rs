use std::sync::Arc;

use anyhow::Result;
use glyphon::{
    Buffer, Cache, FontSystem, Metrics, Shaping, SwashCache, TextAtlas, TextRenderer, Viewport,
};
use wgpu::{
    CompositeAlphaMode, DeviceDescriptor, Instance, InstanceDescriptor, MultisampleState,
    PowerPreference, PresentMode, RequestAdapterOptions, SurfaceConfiguration, TextureUsages,
};
use winit::window::Window;

use crate::config::CONFIG;

use super::font_attrs;

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
    let font_system = FontSystem::new();
    let swash_cache = SwashCache::new();
    let cache = Cache::new(device);
    let viewport = Viewport::new(device, &cache);
    let mut atlas = TextAtlas::new(device, queue, &cache, format);
    let text_renderer = TextRenderer::new(&mut atlas, device, MultisampleState::default(), None);
    TextInit { font_system, swash_cache, viewport, atlas, text_renderer }
}

pub(super) fn metrics_for(scale: f32) -> (f32, f32) {
    let font_size = (CONFIG.font.size_pt * scale).round();
    let line_height = (font_size * CONFIG.font.line_height_factor).round();
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
