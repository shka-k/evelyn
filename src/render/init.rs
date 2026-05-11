use std::sync::Arc;

use anyhow::Result;
use wgpu::{
    CompositeAlphaMode, DeviceDescriptor, Instance, InstanceDescriptor, PowerPreference,
    PresentMode, RequestAdapterOptions, SurfaceConfiguration, TextureUsages,
};
use winit::window::Window;

use crate::config::config;

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

pub(super) fn metrics_for(scale: f32) -> (f32, f32) {
    let cfg = config();
    let font_size = (cfg.font.size_pt * scale).round();
    let line_height = (font_size * cfg.font.line_height_factor).round();
    (font_size, line_height)
}
