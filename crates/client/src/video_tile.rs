//! Live video on screen.
//!
//! On the GPU renderer each video has one texture that's overwritten in place when a new
//! picture arrives, so a frame is swapped atomically and never drawn half-ready. (Handing
//! iced a fresh image per frame made big pictures blink: images over 2 MB are uploaded on
//! a background thread and draw nothing until they land.) The low memory renderer draws
//! images synchronously, so it gets the plain image.
//!
//! Received video comes as YUV (I420) planes on the GPU renderer: the shader turns them
//! into colours, which saves converting every pixel on the CPU (a lot at 1440p 60 fps).

use iced::advanced::image::{self as core_image, Renderer as _};
use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer as core_renderer;
use iced::advanced::widget::{Tree, Widget};
use iced::wgpu;
use iced::widget::shader::{self, Viewport};
use iced::{ContentFit, Element, Length, Rectangle, Size, mouse};
use iced_renderer::fallback;
use std::collections::HashMap;
use std::sync::{Arc, Weak};

/// One video's latest picture, ready to draw.
#[derive(Clone)]
pub struct VideoFrame {
    /// Identifies the video (its texture); dropped when the video goes away.
    pub stream: Arc<()>,
    pub serial: u64,
    pub width: u32,
    pub height: u32,
    /// The picture as RGBA (empty when it comes as YUV planes).
    pub rgba: Arc<Vec<u8>>,
    /// ...or as YUV planes (received video, on the GPU renderer).
    pub i420: Option<Arc<celerity_stream::I420Frame>>,
    /// The RGBA pixels for the low memory renderer.
    pub handle: Option<core_image::Handle>,
}

impl std::fmt::Debug for VideoFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "VideoFrame({}x{} #{})", self.width, self.height, self.serial)
    }
}

pub struct VideoTile {
    frame: VideoFrame,
    fit: ContentFit,
}

pub fn video_tile(frame: &VideoFrame, fit: ContentFit) -> VideoTile {
    VideoTile { frame: frame.clone(), fit }
}

impl<Message, Theme> Widget<Message, Theme, iced::Renderer> for VideoTile {
    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Fill)
    }

    fn layout(&mut self, _tree: &mut Tree, _renderer: &iced::Renderer, limits: &layout::Limits) -> layout::Node {
        layout::Node::new(limits.resolve(Length::Fill, Length::Fill, Size::ZERO))
    }

    fn draw(
        &self,
        _tree: &Tree,
        renderer: &mut iced::Renderer,
        _theme: &Theme,
        _style: &core_renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        let Some(clip) = bounds.intersection(viewport) else { return };
        let picture = Size::new(self.frame.width as f32, self.frame.height as f32);
        if picture.width < 1.0 || picture.height < 1.0 {
            return;
        }
        let fitted = self.fit.fit(picture, bounds.size());
        // Snap to whole pixels so the picture isn't resampled half a pixel off.
        let dest = Rectangle {
            x: (bounds.center_x() - fitted.width / 2.0).round(),
            y: (bounds.center_y() - fitted.height / 2.0).round(),
            width: fitted.width.round(),
            height: fitted.height.round(),
        };
        match renderer {
            fallback::Renderer::Primary(gpu) => {
                use iced::advanced::Renderer as _;
                use iced_wgpu::primitive::Renderer as _;
                gpu.with_layer(clip, |gpu| gpu.draw_primitive(dest, VideoPrimitive(self.frame.clone())));
            }
            other => {
                use iced::advanced::Renderer as _;
                let Some(handle) = self.frame.handle.clone() else { return };
                let image = core_image::Image {
                    handle,
                    filter_method: core_image::FilterMethod::Linear,
                    rotation: iced::Radians(0.0),
                    border_radius: Default::default(),
                    opacity: 1.0,
                    snap: true,
                };
                other.with_layer(clip, |r| r.draw_image(image, dest, clip));
            }
        }
    }
}

impl<'a, Message: 'a, Theme: 'a> From<VideoTile> for Element<'a, Message, Theme, iced::Renderer> {
    fn from(tile: VideoTile) -> Self {
        Element::new(tile)
    }
}

// ---- GPU side

#[derive(Debug)]
struct VideoPrimitive(VideoFrame);

/// One video's textures: RGBA, or Y + U + V.
struct Texture {
    stream: Weak<()>,
    serial: u64,
    size: (u32, u32),
    yuv: bool,
    textures: Vec<wgpu::Texture>,
    bind_group: wgpu::BindGroup,
    used: bool,
}

pub struct VideoPipeline {
    rgba: wgpu::RenderPipeline,
    yuv: wgpu::RenderPipeline,
    rgba_layout: wgpu::BindGroupLayout,
    yuv_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    srgb: bool,
    textures: HashMap<usize, Texture>,
}

fn texture_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

fn sampler_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
        count: None,
    }
}

fn pipeline(
    device: &wgpu::Device,
    module: &wgpu::ShaderModule,
    layout: &wgpu::BindGroupLayout,
    fragment: &str,
    format: wgpu::TextureFormat,
) -> wgpu::RenderPipeline {
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("video tile"),
        bind_group_layouts: &[layout],
        push_constant_ranges: &[],
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("video tile"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module,
            entry_point: Some("vs_main"),
            buffers: &[],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module,
            entry_point: Some(fragment),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleStrip, ..Default::default() },
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview: None,
        cache: None,
    })
}

impl shader::Pipeline for VideoPipeline {
    fn new(device: &wgpu::Device, _queue: &wgpu::Queue, format: wgpu::TextureFormat) -> Self {
        let module = |source: &str| {
            device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("video tile"),
                source: wgpu::ShaderSource::Wgsl(format!("{VERTEX}{source}").into()),
            })
        };
        let (rgba_module, yuv_module) = (module(RGBA_SHADER), module(YUV_SHADER));
        let rgba_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("video tile rgba"),
            entries: &[texture_entry(0), sampler_entry(1)],
        });
        let yuv_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("video tile yuv"),
            entries: &[texture_entry(0), texture_entry(1), texture_entry(2), sampler_entry(3)],
        });
        let srgb = format.is_srgb();
        let rgba = pipeline(device, &rgba_module, &rgba_layout, "fs_rgba", format);
        // YUV gives gamma-encoded colours; an sRGB target expects linear ones.
        let yuv = pipeline(device, &yuv_module, &yuv_layout, if srgb { "fs_yuv_linear" } else { "fs_yuv" }, format);
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("video tile"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        VideoPipeline { rgba, yuv, rgba_layout, yuv_layout, sampler, srgb, textures: HashMap::new() }
    }

    fn trim(&mut self) {
        // Videos not drawn this frame give their memory back (they re-upload if shown again).
        self.textures.retain(|_, t| std::mem::take(&mut t.used) && t.stream.strong_count() > 0);
    }
}

fn make_texture(device: &wgpu::Device, (w, h): (u32, u32), format: wgpu::TextureFormat) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("video tile"),
        size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

fn write(queue: &wgpu::Queue, texture: &wgpu::Texture, data: &[u8], stride: u32, (w, h): (u32, u32)) {
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        data,
        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(stride), rows_per_image: Some(h) },
        wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
    );
}

impl VideoPipeline {
    fn create(&self, device: &wgpu::Device, stream: Weak<()>, size: (u32, u32), yuv: bool) -> Texture {
        let (w, h) = size;
        let textures: Vec<wgpu::Texture> = if yuv {
            let half = (w.div_ceil(2), h.div_ceil(2));
            vec![
                make_texture(device, size, wgpu::TextureFormat::R8Unorm),
                make_texture(device, half, wgpu::TextureFormat::R8Unorm),
                make_texture(device, half, wgpu::TextureFormat::R8Unorm),
            ]
        } else {
            let format = if self.srgb { wgpu::TextureFormat::Rgba8UnormSrgb } else { wgpu::TextureFormat::Rgba8Unorm };
            vec![make_texture(device, size, format)]
        };
        let views: Vec<wgpu::TextureView> =
            textures.iter().map(|t| t.create_view(&wgpu::TextureViewDescriptor::default())).collect();
        let mut entries: Vec<wgpu::BindGroupEntry> = views
            .iter()
            .enumerate()
            .map(|(i, v)| wgpu::BindGroupEntry { binding: i as u32, resource: wgpu::BindingResource::TextureView(v) })
            .collect();
        entries.push(wgpu::BindGroupEntry {
            binding: views.len() as u32,
            resource: wgpu::BindingResource::Sampler(&self.sampler),
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("video tile"),
            layout: if yuv { &self.yuv_layout } else { &self.rgba_layout },
            entries: &entries,
        });
        Texture { stream, serial: u64::MAX, size, yuv, textures, bind_group, used: false }
    }
}

impl shader::Primitive for VideoPrimitive {
    type Pipeline = VideoPipeline;

    fn prepare(
        &self,
        pipeline: &mut VideoPipeline,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _bounds: &Rectangle,
        _viewport: &Viewport,
    ) {
        let f = &self.0;
        let key = Arc::as_ptr(&f.stream) as usize;
        let yuv = f.i420.is_some();
        let size = match &f.i420 {
            Some(p) => (p.width(), p.height()),
            None => (f.width, f.height),
        };
        if size.0 == 0 || size.1 == 0 || (!yuv && f.rgba.len() < (size.0 * size.1 * 4) as usize) {
            return;
        }
        let stale =
            pipeline.textures.get(&key).is_none_or(|t| t.size != size || t.yuv != yuv || t.stream.strong_count() == 0);
        if stale {
            let t = pipeline.create(device, Arc::downgrade(&f.stream), size, yuv);
            pipeline.textures.insert(key, t);
        }
        let Some(t) = pipeline.textures.get_mut(&key) else { return };
        t.used = true;
        if t.serial == f.serial {
            return;
        }
        t.serial = f.serial;
        match &f.i420 {
            Some(p) => {
                let half = (size.0.div_ceil(2), size.1.div_ceil(2));
                write(queue, &t.textures[0], p.y(), p.y_stride(), size);
                write(queue, &t.textures[1], p.u(), p.uv_stride(), half);
                write(queue, &t.textures[2], p.v(), p.uv_stride(), half);
            }
            None => write(queue, &t.textures[0], &f.rgba[..(size.0 * size.1 * 4) as usize], size.0 * 4, size),
        }
    }

    fn draw(&self, pipeline: &VideoPipeline, pass: &mut wgpu::RenderPass<'_>) -> bool {
        let key = Arc::as_ptr(&self.0.stream) as usize;
        let Some(t) = pipeline.textures.get(&key) else { return true };
        pass.set_pipeline(if t.yuv { &pipeline.yuv } else { &pipeline.rgba });
        pass.set_bind_group(0, &t.bind_group, &[]);
        pass.draw(0..4, 0..1);
        true
    }
}

/// A quad covering the primitive's bounds (the viewport is set to them).
const VERTEX: &str = r#"
struct Out {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) i: u32) -> Out {
    let uv = vec2<f32>(f32(i & 1u), f32((i >> 1u) & 1u));
    var out: Out;
    out.pos = vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
    out.uv = uv;
    return out;
}
"#;

/// Colours from an RGBA texture.
const RGBA_SHADER: &str = r#"
@group(0) @binding(0) var picture: texture_2d<f32>;
@group(0) @binding(1) var smooth_sampler: sampler;

@fragment
fn fs_rgba(in: Out) -> @location(0) vec4<f32> {
    return vec4<f32>(textureSample(picture, smooth_sampler, in.uv).rgb, 1.0);
}
"#;

/// Colours from BT.709 limited-range YUV planes (what the encoder sends).
const YUV_SHADER: &str = r#"
@group(0) @binding(0) var plane_y: texture_2d<f32>;
@group(0) @binding(1) var plane_u: texture_2d<f32>;
@group(0) @binding(2) var plane_v: texture_2d<f32>;
@group(0) @binding(3) var yuv_sampler: sampler;

fn yuv_rgb(uv: vec2<f32>) -> vec3<f32> {
    let y = (textureSample(plane_y, yuv_sampler, uv).r - 16.0 / 255.0) * (255.0 / 219.0);
    let u = (textureSample(plane_u, yuv_sampler, uv).r - 128.0 / 255.0) * (255.0 / 224.0);
    let v = (textureSample(plane_v, yuv_sampler, uv).r - 128.0 / 255.0) * (255.0 / 224.0);
    let rgb = vec3<f32>(y + 1.5748 * v, y - 0.1873 * u - 0.4681 * v, y + 1.8556 * u);
    return clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0));
}

@fragment
fn fs_yuv(in: Out) -> @location(0) vec4<f32> {
    return vec4<f32>(yuv_rgb(in.uv), 1.0);
}

@fragment
fn fs_yuv_linear(in: Out) -> @location(0) vec4<f32> {
    let c = yuv_rgb(in.uv);
    let lo = c / 12.92;
    let hi = pow((c + 0.055) / 1.055, vec3<f32>(2.4));
    return vec4<f32>(select(hi, lo, c <= vec3<f32>(0.04045)), 1.0);
}
"#;
