use bytemuck::{Pod, Zeroable};
use gpui::ScaledClientFrame;

const CLIENT_FRAME_SHADERS: &str = include_str!("shaders_client_frame.wgsl");

/// Clears the pixels outside a client-decorated window's rounded shape.
pub(crate) struct ClientFrameRenderer {
    pipeline: wgpu::RenderPipeline,
    uniforms: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
}

/// Matches `ClientFrameUniforms` in the shader.
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
#[repr(C)]
struct ClientFrameUniforms {
    /// The shape's bounds as origin and size.
    bounds: [f32; 4],
    /// Top-left, top-right, bottom-right, and bottom-left radii.
    corner_radii: [f32; 4],
}

impl ClientFrameUniforms {
    fn new(frame: &ScaledClientFrame) -> Self {
        let bounds = frame.bounds;
        let radii = &frame.corner_radii;
        Self {
            bounds: [
                bounds.origin.x.0,
                bounds.origin.y.0,
                bounds.size.width.0,
                bounds.size.height.0,
            ],
            corner_radii: [
                radii.top_left.0,
                radii.top_right.0,
                radii.bottom_right.0,
                radii.bottom_left.0,
            ],
        }
    }
}

impl ClientFrameRenderer {
    pub(crate) fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("client_frame_layout"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: wgpu::BufferSize::new(
                            size_of::<ClientFrameUniforms>() as u64
                        ),
                    },
                    count: None,
                }],
            });
        let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("client_frame_uniforms"),
            size: size_of::<ClientFrameUniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("client_frame_bind_group"),
            layout: &bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniforms.as_entire_binding(),
            }],
        });
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("client_frame_shaders"),
            source: wgpu::ShaderSource::Wgsl(CLIENT_FRAME_SHADERS.into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("client_frame_pipeline_layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });
        // Multiplies color and alpha by the shape's coverage, which the fragment shader
        // returns as alpha. The target holds premultiplied color in every alpha mode because
        // frames start from a transparent clear.
        let mask = wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::Zero,
            dst_factor: wgpu::BlendFactor::SrcAlpha,
            operation: wgpu::BlendOperation::Add,
        };
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("client_frame_mask"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_client_frame"),
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_client_frame"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState {
                        color: mask,
                        alpha: mask,
                    }),
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
        Self {
            pipeline,
            uniforms,
            bind_group,
        }
    }

    /// Writes the uniforms of `frame`, which [`Self::draw_mask`] uses.
    pub(crate) fn prepare(&self, queue: &wgpu::Queue, frame: &ScaledClientFrame) {
        queue.write_buffer(
            &self.uniforms,
            0,
            bytemuck::bytes_of(&ClientFrameUniforms::new(frame)),
        );
    }

    /// Clears the pixels outside the shape of the frame passed to [`Self::prepare`].
    pub(crate) fn draw_mask(&self, pass: &mut wgpu::RenderPass<'_>) {
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.draw(0..3, 0..1);
    }
}

#[cfg(test)]
mod tests {
    use super::{CLIENT_FRAME_SHADERS, ClientFrameUniforms};
    use gpui::{Bounds, Corners, ScaledClientFrame, ScaledPixels, Size, point};

    #[test]
    fn client_frame_shader_is_valid_wgsl() {
        let module =
            naga::front::wgsl::parse_str(CLIENT_FRAME_SHADERS).expect("shader should parse");
        naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::empty(),
        )
        .validate(&module)
        .expect("shader should validate");
    }

    #[test]
    fn uniforms_order_corners_clockwise_from_top_left() {
        let frame = ScaledClientFrame {
            bounds: Bounds::new(
                point(ScaledPixels(1.0), ScaledPixels(2.0)),
                Size::new(ScaledPixels(3.0), ScaledPixels(4.0)),
            ),
            corner_radii: Corners {
                top_left: ScaledPixels(5.0),
                top_right: ScaledPixels(6.0),
                bottom_right: ScaledPixels(7.0),
                bottom_left: ScaledPixels(8.0),
            },
            shadows: Vec::new(),
        };
        assert_eq!(
            ClientFrameUniforms::new(&frame),
            ClientFrameUniforms {
                bounds: [1.0, 2.0, 3.0, 4.0],
                corner_radii: [5.0, 6.0, 7.0, 8.0],
            }
        );
    }
}
