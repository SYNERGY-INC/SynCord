//! Live video on screen.
//!
//! On the GPU renderer each video has one texture that's overwritten in place when a new
//! picture arrives, so a frame is swapped atomically and never drawn half-ready. (Handing
//! iced a fresh image per frame made big pictures blink: images over 2 MB are uploaded on
//! a background thread and draw nothing until they land.) The low memory renderer draws
//! images synchronously, so it gets the plain image.

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
    pub rgba: Arc<Vec<u8>>,
    /// The same pixels for the low memory renderer.
    pub handle: core_image::Handle,
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
                let image = core_image::Image {
                    handle: self.frame.handle.clone(),
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

struct Texture {
    stream: Weak<()>,
    serial: u64,
    size: (u32, u32),
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    used: bool,
}

pub struct VideoPipeline {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    srgb: bool,
    textures: HashMap<usize, Texture>,
}

impl shader::Pipeline for VideoPipeline {
    fn new(device: &wgpu::Device, _queue: &wgpu::Queue, format: wgpu::TextureFormat) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("video tile"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("video tile"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("video tile"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("video tile"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_main"),
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
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("video tile"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        VideoPipeline { pipeline, layout, sampler, srgb: format.is_srgb(), textures: HashMap::new() }
    }

    fn trim(&mut self) {
        // Videos not drawn this frame give their memory back (they re-upload if shown again).
        self.textures.retain(|_, t| std::mem::take(&mut t.used) && t.stream.strong_count() > 0);
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
        let size = (f.width, f.height);
        if f.rgba.len() < (f.width * f.height * 4) as usize {
            return;
        }
        let stale = pipeline.textures.get(&key).is_none_or(|t| t.size != size || t.stream.strong_count() == 0);
        if stale {
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("video tile"),
                size: wgpu::Extent3d { width: f.width, height: f.height, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: if pipeline.srgb {
                    wgpu::TextureFormat::Rgba8UnormSrgb
                } else {
                    wgpu::TextureFormat::Rgba8Unorm
                },
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("video tile"),
                layout: &pipeline.layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&pipeline.sampler) },
                ],
            });
            pipeline.textures.insert(
                key,
                Texture { stream: Arc::downgrade(&f.stream), serial: u64::MAX, size, texture, bind_group, used: false },
            );
        }
        let Some(t) = pipeline.textures.get_mut(&key) else { return };
        t.used = true;
        if t.serial != f.serial {
            t.serial = f.serial;
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &t.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &f.rgba[..(f.width * f.height * 4) as usize],
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(f.width * 4),
                    rows_per_image: Some(f.height),
                },
                wgpu::Extent3d { width: f.width, height: f.height, depth_or_array_layers: 1 },
            );
        }
    }

    fn draw(&self, pipeline: &VideoPipeline, pass: &mut wgpu::RenderPass<'_>) -> bool {
        let key = Arc::as_ptr(&self.0.stream) as usize;
        let Some(t) = pipeline.textures.get(&key) else { return true };
        pass.set_pipeline(&pipeline.pipeline);
        pass.set_bind_group(0, &t.bind_group, &[]);
        pass.draw(0..4, 0..1);
        true
    }
}

/// A quad covering the primitive's bounds (the viewport is set to them).
const SHADER: &str = r#"
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

@group(0) @binding(0) var picture: texture_2d<f32>;
@group(0) @binding(1) var smooth_sampler: sampler;

@fragment
fn fs_main(in: Out) -> @location(0) vec4<f32> {
    return vec4<f32>(textureSample(picture, smooth_sampler, in.uv).rgb, 1.0);
}
"#;
