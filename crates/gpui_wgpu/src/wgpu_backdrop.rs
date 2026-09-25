use gpui::{
    BackdropFilter, BackdropPass, BackdropScratchSize, BackdropUniforms, DevicePixels, Scene, Size,
};
use std::num::NonZeroU64;

const BACKDROP_SHADERS: &str = include_str!("shaders_backdrop.wgsl");

/// Renders backdrop filters into the texture a scene draws to.
pub(crate) struct BackdropRenderer {
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    format: wgpu::TextureFormat,
    uniform_stride: u64,
    frame: Option<Frame>,
    warned_copy_unsupported: bool,
}

/// Resources of the frames that draw filters. They grow to the largest snapshot and uniform
/// data, and are released when a frame has no filters.
struct Frame {
    scratch_size: BackdropScratchSize,
    snapshot: ScratchTexture,
    horizontal: ScratchTexture,
    vertical: ScratchTexture,
    /// The uniforms of every pass, `uniform_stride` bytes apart.
    uniforms: wgpu::Buffer,
    /// Bind groups that read the snapshot, horizontal, and vertical textures.
    reads_snapshot: wgpu::BindGroup,
    reads_horizontal: wgpu::BindGroup,
    reads_vertical: wgpu::BindGroup,
    /// The first uniform slot of each filter in the scene.
    first_slots: Vec<u32>,
}

struct ScratchTexture {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
}

impl BackdropRenderer {
    pub(crate) fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("backdrop_layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: true,
                        min_binding_size: NonZeroU64::new(size_of::<BackdropUniforms>() as u64),
                    },
                    count: None,
                },
                texture_entry(1),
                texture_entry(2),
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("backdrop_shaders"),
            source: wgpu::ShaderSource::Wgsl(BACKDROP_SHADERS.into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("backdrop_pipeline_layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("backdrop_filter"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_backdrop"),
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_backdrop"),
                // The composite pass replaces premultiplied color and alpha.
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("backdrop_sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let alignment = u64::from(device.limits().min_uniform_buffer_offset_alignment);
        Self {
            pipeline,
            bind_group_layout,
            sampler,
            format,
            uniform_stride: (size_of::<BackdropUniforms>() as u64).next_multiple_of(alignment),
            frame: None,
            warned_copy_unsupported: false,
        }
    }

    /// Sizes the scratch textures and writes the uniforms for the filters in `scene`, which
    /// draws to `target`.
    pub(crate) fn prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        scene: &Scene,
        target: &wgpu::Texture,
    ) {
        let viewport = texture_size(target);
        let Some(required) = scene.backdrop_scratch_size(viewport) else {
            self.frame = None;
            return;
        };
        if !target.usage().contains(wgpu::TextureUsages::COPY_SRC) {
            if !self.warned_copy_unsupported {
                self.warned_copy_unsupported = true;
                log::warn!("the render target cannot be copied, so backdrop filters are skipped");
            }
            self.frame = None;
            return;
        }

        let scratch_size = self
            .frame
            .as_ref()
            .map_or(required, |frame| BackdropScratchSize {
                snapshot: frame.scratch_size.snapshot.max(&required.snapshot),
                blur: frame.scratch_size.blur.max(&required.blur),
            });
        let mut first_slots = Vec::with_capacity(scene.backdrop_filters.len());
        let mut uniforms = Vec::new();
        for filter in &scene.backdrop_filters {
            first_slots.push((uniforms.len() as u64 / self.uniform_stride) as u32);
            let Some(snapshot) = filter.snapshot_bounds(viewport) else {
                continue;
            };
            for &pass in filter.passes() {
                let pass_uniforms = filter.uniforms(pass, snapshot, scratch_size, viewport);
                // SAFETY: `BackdropUniforms` is `repr(C)` plain data without padding.
                uniforms.extend_from_slice(unsafe {
                    std::slice::from_raw_parts(
                        (&raw const pass_uniforms).cast::<u8>(),
                        size_of::<BackdropUniforms>(),
                    )
                });
                uniforms.resize(
                    (uniforms.len() as u64).next_multiple_of(self.uniform_stride) as usize,
                    0,
                );
            }
        }

        let uniform_capacity = self.frame.as_ref().map_or(0, |frame| frame.uniforms.size());
        let reusable = self.frame.take().filter(|frame| {
            frame.scratch_size == scratch_size && uniform_capacity >= uniforms.len() as u64
        });
        let mut frame = match reusable {
            Some(frame) => frame,
            None => {
                let uniform_capacity =
                    uniform_capacity.max((uniforms.len() as u64).next_power_of_two());
                self.create_frame(device, scratch_size, uniform_capacity)
            }
        };
        frame.first_slots = first_slots;
        queue.write_buffer(&frame.uniforms, 0, &uniforms);
        self.frame = Some(frame);
    }

    fn create_frame(
        &self,
        device: &wgpu::Device,
        scratch_size: BackdropScratchSize,
        uniform_capacity: u64,
    ) -> Frame {
        let texture = |label: &str, width: i32, height: i32, copy_destination: bool| {
            let mut usage =
                wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING;
            if copy_destination {
                usage |= wgpu::TextureUsages::COPY_DST;
            }
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: width as u32,
                    height: height as u32,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: self.format,
                usage,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            ScratchTexture { texture, view }
        };
        let (width, height) = (
            scratch_size.snapshot.width.0,
            scratch_size.snapshot.height.0,
        );
        let (blur_width, blur_height) = (scratch_size.blur.width.0, scratch_size.blur.height.0);
        let snapshot = texture("backdrop_snapshot", width, height, true);
        let horizontal = texture("backdrop_horizontal", blur_width, blur_height, false);
        let vertical = texture("backdrop_vertical", blur_width, blur_height, false);
        let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("backdrop_uniforms"),
            size: uniform_capacity,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bind_group = |label: &str, source: &ScratchTexture| {
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some(label),
                layout: &self.bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                            buffer: &uniforms,
                            offset: 0,
                            size: NonZeroU64::new(size_of::<BackdropUniforms>() as u64),
                        }),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&source.view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::TextureView(&snapshot.view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                ],
            })
        };
        Frame {
            reads_snapshot: bind_group("backdrop_reads_snapshot", &snapshot),
            reads_horizontal: bind_group("backdrop_reads_horizontal", &horizontal),
            reads_vertical: bind_group("backdrop_reads_vertical", &vertical),
            scratch_size,
            snapshot,
            horizontal,
            vertical,
            uniforms,
            first_slots: Vec::new(),
        }
    }

    /// Encodes the filter at `index` in the scene passed to [`Self::prepare`]. No render
    /// pass may be open on `encoder`.
    pub(crate) fn encode(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::Texture,
        target_view: &wgpu::TextureView,
        index: usize,
        filter: &BackdropFilter,
    ) {
        let viewport = texture_size(target);
        let (Some(frame), Some(snapshot_bounds)) = (&self.frame, filter.snapshot_bounds(viewport))
        else {
            return;
        };
        let Some(&first_slot) = frame.first_slots.get(index) else {
            return;
        };

        encoder.copy_texture_to_texture(
            wgpu::TexelCopyTextureInfo {
                texture: target,
                mip_level: 0,
                origin: wgpu::Origin3d {
                    x: snapshot_bounds.origin.x.0 as u32,
                    y: snapshot_bounds.origin.y.0 as u32,
                    z: 0,
                },
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyTextureInfo {
                texture: &frame.snapshot.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::Extent3d {
                width: snapshot_bounds.size.width.0 as u32,
                height: snapshot_bounds.size.height.0 as u32,
                depth_or_array_layers: 1,
            },
        );

        for (slot, &pass) in (first_slot..).zip(filter.passes()) {
            let (pass_target, bind_group) = match pass {
                BackdropPass::Horizontal => (&frame.horizontal.view, &frame.reads_snapshot),
                BackdropPass::Vertical => (&frame.vertical.view, &frame.reads_horizontal),
                BackdropPass::Composite if filter.radius.0 > 0.0 => {
                    (target_view, &frame.reads_vertical)
                }
                BackdropPass::Composite => (target_view, &frame.reads_snapshot),
            };
            let Some(scissor) = filter.pass_scissor(pass, snapshot_bounds, viewport) else {
                continue;
            };
            let load = match pass {
                BackdropPass::Composite => wgpu::LoadOp::Load,
                _ => wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
            };
            let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("backdrop_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: pass_target,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load,
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                ..Default::default()
            });
            render_pass.set_pipeline(&self.pipeline);
            render_pass.set_scissor_rect(
                scissor.origin.x.0 as u32,
                scissor.origin.y.0 as u32,
                scissor.size.width.0 as u32,
                scissor.size.height.0 as u32,
            );
            let offset = u64::from(slot) * self.uniform_stride;
            render_pass.set_bind_group(0, bind_group, &[offset as u32]);
            render_pass.draw(0..3, 0..1);
        }
    }
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

fn texture_size(texture: &wgpu::Texture) -> Size<DevicePixels> {
    Size {
        width: DevicePixels(texture.width() as i32),
        height: DevicePixels(texture.height() as i32),
    }
}

#[cfg(test)]
mod tests {
    use super::BACKDROP_SHADERS;

    #[test]
    fn backdrop_shader_is_valid_wgsl() {
        let module = naga::front::wgsl::parse_str(BACKDROP_SHADERS).expect("shader should parse");
        naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::empty(),
        )
        .validate(&module)
        .expect("shader should validate");
    }
}
