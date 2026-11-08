//! Direct upload of borrowed PipeWire memory. No application-side pixel allocation,
//! crop copy, or CPU channel conversion. All outputs share the resulting texture.

use crate::gpu::Gpu;
use anyhow::Result;
use lpmt_media::{CaptureFrame, CapturePixelFormat};
use std::{collections::HashMap, sync::Arc};

pub struct CaptureTexture {
    pub texture: wgpu::Texture,
    pub view: wgpu::TextureView,
}

struct Slot {
    format: CapturePixelFormat,
    input: wgpu::Texture,
    convert: Option<wgpu::BindGroup>,
    render_view: wgpu::TextureView,
    output: Arc<CaptureTexture>,
}

/// Owned by the capture worker. wgpu device/queue handles share the application's
/// device, so writes, conversion and draws use one synchronized command queue.
pub struct CaptureUploader {
    device: wgpu::Device,
    queue: wgpu::Queue,
    layout: wgpu::BindGroupLayout,
    opaque_pipeline: wgpu::RenderPipeline,
    slots: HashMap<usize, Slot>,
}

impl CaptureUploader {
    pub fn new(gpu: &Gpu) -> Self {
        let device = gpu.device().clone();
        let queue = gpu.queue().clone();
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("capture-convert-layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            }],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("capture-convert-layout"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        // Use unorm input/output for byte-exact RGB; sample the result with an
        // sRGB view in the existing renderer (no double gamma conversion).
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("capture-force-opaque"),
            source: wgpu::ShaderSource::Wgsl(
                r#"
@group(0) @binding(0) var source: texture_2d<f32>;
@vertex fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    let p = array(vec2(-1.0, -1.0), vec2(3.0, -1.0), vec2(-1.0, 3.0));
    return vec4(p[i], 0.0, 1.0);
}
@fragment fn fs(@builtin(position) p: vec4<f32>) -> @location(0) vec4<f32> {
    return vec4(textureLoad(source, vec2<i32>(p.xy), 0).rgb, 1.0);
}
"#
                .into(),
            ),
        });
        let opaque_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("capture-force-opaque"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        Self {
            device,
            queue,
            layout,
            opaque_pipeline,
            slots: HashMap::new(),
        }
    }

    pub fn upload(&mut self, index: usize, frame: CaptureFrame<'_>) -> Result<Arc<CaptureTexture>> {
        frame.validate()?;
        let max = self.device.limits().max_texture_dimension_2d;
        anyhow::ensure!(
            frame.width <= max && frame.height <= max,
            "Window exceeds GPU texture limit {max}"
        );
        let recreate = self.slots.get(&index).is_none_or(|slot| {
            slot.format != frame.format
                || slot.input.width() != frame.width
                || slot.input.height() != frame.height
        });
        if recreate {
            self.slots.insert(index, self.create_slot(frame));
        }
        let slot = &self.slots[&index];
        // write_texture copies the borrowed bytes into wgpu-owned staging memory
        // before returning. The PipeWire buffer can then be requeued safely.
        self.queue.write_texture(
            slot.input.as_image_copy(),
            frame.data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(frame.stride),
                rows_per_image: None,
            },
            slot.input.size(),
        );
        if let Some(bind_group) = &slot.convert {
            let mut encoder = self
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("capture-convert"),
                });
            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("capture-convert"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &slot.render_view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    ..Default::default()
                });
                pass.set_pipeline(&self.opaque_pipeline);
                pass.set_bind_group(0, bind_group, &[]);
                pass.draw(0..3, 0..1);
            }
            self.queue.submit([encoder.finish()]);
        } else {
            self.queue.submit([]);
        }
        Ok(slot.output.clone())
    }

    fn create_slot(&self, frame: CaptureFrame<'_>) -> Slot {
        let opaque = matches!(
            frame.format,
            CapturePixelFormat::Rgbx | CapturePixelFormat::Bgrx
        );
        let bgra = matches!(
            frame.format,
            CapturePixelFormat::Bgra | CapturePixelFormat::Bgrx
        );
        let format = if bgra {
            wgpu::TextureFormat::Bgra8Unorm
        } else {
            wgpu::TextureFormat::Rgba8Unorm
        };
        let size = wgpu::Extent3d {
            width: frame.width,
            height: frame.height,
            depth_or_array_layers: 1,
        };
        let input = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("capture-native-pixels"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[format.add_srgb_suffix()],
        });
        let (texture, convert) = if opaque {
            let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("capture-opaque-pixels"),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[wgpu::TextureFormat::Rgba8UnormSrgb],
            });
            let view = input.create_view(&wgpu::TextureViewDescriptor::default());
            let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("capture-convert"),
                layout: &self.layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                }],
            });
            (texture, Some(bind_group))
        } else {
            (input.clone(), None)
        };
        let view = texture.create_view(&wgpu::TextureViewDescriptor {
            format: Some(texture.format().add_srgb_suffix()),
            ..Default::default()
        });
        let render_view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Slot {
            render_view,
            format: frame.format,
            input,
            convert,
            output: Arc::new(CaptureTexture { texture, view }),
        }
    }
}

impl lpmt_media::CaptureConsumer for CaptureUploader {
    type Frame = Arc<CaptureTexture>;
    fn upload(&mut self, index: usize, frame: CaptureFrame<'_>) -> Result<Self::Frame> {
        CaptureUploader::upload(self, index, frame)
    }
    fn release(&mut self, index: usize) {
        self.slots.remove(&index);
    }
}
