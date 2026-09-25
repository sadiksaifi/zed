use gpui::{
    BackdropFilter, BackdropScratchSize, BackdropUniforms, Bounds, DevicePixels, Scene, Size,
};
use metal::MTLPixelFormat;

/// Renders backdrop filters into the target a scene draws to.
pub(super) struct BackdropRenderer {
    pipeline: metal::RenderPipelineState,
    scratch: Option<Scratch>,
}

/// Textures reused by every filter in a frame. They grow to the largest snapshot and are
/// released when a frame has no filters.
struct Scratch {
    size: BackdropScratchSize,
    snapshot: metal::Texture,
    horizontal: metal::Texture,
    vertical: metal::Texture,
}

impl BackdropRenderer {
    pub(super) fn new(device: &metal::DeviceRef, library: &metal::LibraryRef) -> Self {
        let descriptor = metal::RenderPipelineDescriptor::new();
        descriptor.set_label("backdrop_filter");
        let vertex = library
            .get_function("backdrop_vertex", None)
            .expect("error locating backdrop_vertex");
        let fragment = library
            .get_function("backdrop_fragment", None)
            .expect("error locating backdrop_fragment");
        descriptor.set_vertex_function(Some(&vertex));
        descriptor.set_fragment_function(Some(&fragment));
        let color = descriptor.color_attachments().object_at(0).unwrap();
        color.set_pixel_format(MTLPixelFormat::BGRA8Unorm);
        // The composite pass replaces premultiplied color and alpha.
        color.set_blending_enabled(false);
        Self {
            pipeline: device
                .new_render_pipeline_state(&descriptor)
                .expect("could not create backdrop pipeline state"),
            scratch: None,
        }
    }

    /// Sizes the scratch textures for the filters in `scene`.
    pub(super) fn prepare(
        &mut self,
        device: &metal::DeviceRef,
        scene: &Scene,
        viewport: Size<DevicePixels>,
    ) {
        let Some(required) = scene.backdrop_scratch_size(viewport) else {
            self.scratch = None;
            return;
        };
        if let Some(scratch) = &self.scratch
            && scratch.size.snapshot.width >= required.snapshot.width
            && scratch.size.snapshot.height >= required.snapshot.height
            && scratch.size.blur.width >= required.blur.width
            && scratch.size.blur.height >= required.blur.height
        {
            return;
        }
        let size = self
            .scratch
            .as_ref()
            .map_or(required, |scratch| BackdropScratchSize {
                snapshot: scratch.size.snapshot.max(&required.snapshot),
                blur: scratch.size.blur.max(&required.blur),
            });
        let texture = |label: &str, width: u64, height: u64| {
            let descriptor = metal::TextureDescriptor::new();
            descriptor.set_width(width);
            descriptor.set_height(height);
            descriptor.set_pixel_format(MTLPixelFormat::BGRA8Unorm);
            descriptor.set_storage_mode(metal::MTLStorageMode::Private);
            descriptor.set_usage(
                metal::MTLTextureUsage::ShaderRead | metal::MTLTextureUsage::RenderTarget,
            );
            let texture = device.new_texture(&descriptor);
            texture.set_label(label);
            texture
        };
        self.scratch = Some(Scratch {
            size,
            snapshot: texture(
                "backdrop_snapshot",
                size.snapshot.width.0 as u64,
                size.snapshot.height.0 as u64,
            ),
            horizontal: texture(
                "backdrop_horizontal",
                size.blur.width.0 as u64,
                size.blur.height.0 as u64,
            ),
            vertical: texture(
                "backdrop_vertical",
                size.blur.width.0 as u64,
                size.blur.height.0 as u64,
            ),
        });
    }

    /// Encodes `filter` into `command_buffer`. No render encoder may be open on it.
    pub(super) fn encode(
        &self,
        command_buffer: &metal::CommandBufferRef,
        target: &metal::TextureRef,
        filter: &BackdropFilter,
        viewport: Size<DevicePixels>,
    ) {
        let (Some(scratch), Some(snapshot_bounds)) =
            (&self.scratch, filter.snapshot_bounds(viewport))
        else {
            return;
        };

        let copy = command_buffer.new_blit_command_encoder();
        copy.copy_from_texture(
            target,
            0,
            0,
            metal::MTLOrigin {
                x: snapshot_bounds.origin.x.0 as u64,
                y: snapshot_bounds.origin.y.0 as u64,
                z: 0,
            },
            metal::MTLSize {
                width: snapshot_bounds.size.width.0 as u64,
                height: snapshot_bounds.size.height.0 as u64,
                depth: 1,
            },
            &scratch.snapshot,
            0,
            0,
            metal::MTLOrigin { x: 0, y: 0, z: 0 },
        );
        copy.end_encoding();

        for &pass in filter.passes() {
            let (pass_target, source): (&metal::TextureRef, &metal::TextureRef) = match pass {
                gpui::BackdropPass::Horizontal => (&scratch.horizontal, &scratch.snapshot),
                gpui::BackdropPass::Vertical => (&scratch.vertical, &scratch.horizontal),
                gpui::BackdropPass::Composite if filter.radius.0 > 0.0 => {
                    (target, &scratch.vertical)
                }
                gpui::BackdropPass::Composite => (target, &scratch.snapshot),
            };
            let Some(scissor) = filter.pass_scissor(pass, snapshot_bounds, viewport) else {
                continue;
            };
            let uniforms = filter.uniforms(pass, snapshot_bounds, scratch.size, viewport);

            let descriptor = metal::RenderPassDescriptor::new();
            let color = descriptor.color_attachments().object_at(0).unwrap();
            color.set_texture(Some(pass_target));
            color.set_load_action(match pass {
                gpui::BackdropPass::Composite => metal::MTLLoadAction::Load,
                _ => metal::MTLLoadAction::DontCare,
            });
            color.set_store_action(metal::MTLStoreAction::Store);
            let encoder = command_buffer.new_render_command_encoder(descriptor);
            encoder.set_render_pipeline_state(&self.pipeline);
            encoder.set_viewport(metal::MTLViewport {
                originX: 0.0,
                originY: 0.0,
                width: pass_target.width() as f64,
                height: pass_target.height() as f64,
                znear: 0.0,
                zfar: 1.0,
            });
            encoder.set_scissor_rect(scissor_rect(scissor));
            encoder.set_fragment_bytes(
                0,
                size_of::<BackdropUniforms>() as u64,
                (&uniforms as *const BackdropUniforms).cast(),
            );
            encoder.set_fragment_texture(0, Some(source));
            encoder.set_fragment_texture(1, Some(&scratch.snapshot));
            encoder.draw_primitives(metal::MTLPrimitiveType::Triangle, 0, 3);
            encoder.end_encoding();
        }
    }
}

fn scissor_rect(bounds: Bounds<DevicePixels>) -> metal::MTLScissorRect {
    metal::MTLScissorRect {
        x: bounds.origin.x.0 as u64,
        y: bounds.origin.y.0 as u64,
        width: bounds.size.width.0 as u64,
        height: bounds.size.height.0 as u64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{ContentMask, Corners, ScaledPixels, point, rgba, size};

    struct Harness {
        device: metal::Device,
        queue: metal::CommandQueue,
        renderer: BackdropRenderer,
    }

    impl Harness {
        fn new() -> Self {
            let device = metal::Device::system_default().expect("native Metal device required");
            #[cfg(not(feature = "runtime_shaders"))]
            let library = device
                .new_library_with_data(super::super::SHADERS_METALLIB)
                .unwrap();
            #[cfg(feature = "runtime_shaders")]
            let library = device
                .new_library_with_source(
                    super::super::SHADERS_SOURCE_FILE,
                    &metal::CompileOptions::new(),
                )
                .unwrap();
            let renderer = BackdropRenderer::new(&device, &library);
            Self {
                queue: device.new_command_queue(),
                device,
                renderer,
            }
        }

        fn target(&self, width: u64, height: u64) -> metal::Texture {
            let descriptor = metal::TextureDescriptor::new();
            descriptor.set_width(width);
            descriptor.set_height(height);
            descriptor.set_pixel_format(MTLPixelFormat::BGRA8Unorm);
            descriptor.set_storage_mode(if self.device.has_unified_memory() {
                metal::MTLStorageMode::Shared
            } else {
                metal::MTLStorageMode::Managed
            });
            descriptor.set_usage(
                metal::MTLTextureUsage::RenderTarget | metal::MTLTextureUsage::ShaderRead,
            );
            self.device.new_texture(&descriptor)
        }

        /// Writes `input` to `target`, filters it, and returns the target's pixels.
        fn filter(
            &mut self,
            target: &metal::TextureRef,
            input: &[u8],
            filter: &BackdropFilter,
        ) -> Vec<u8> {
            let (width, height) = (target.width(), target.height());
            let region = metal::MTLRegion::new_2d(0, 0, width, height);
            target.replace_region(region, 0, input.as_ptr().cast(), width * 4);
            let viewport = size(DevicePixels(width as i32), DevicePixels(height as i32));
            let mut scene = Scene::default();
            scene.insert_primitive(*filter);
            scene.finish();
            self.renderer.prepare(&self.device, &scene, viewport);

            let command_buffer = self.queue.new_command_buffer();
            for filter in &scene.backdrop_filters {
                self.renderer
                    .encode(command_buffer, target, filter, viewport);
            }
            if !self.device.has_unified_memory() {
                let sync = command_buffer.new_blit_command_encoder();
                sync.synchronize_resource(target);
                sync.end_encoding();
            }
            command_buffer.commit();
            command_buffer.wait_until_completed();
            assert_eq!(
                command_buffer.status(),
                metal::MTLCommandBufferStatus::Completed
            );
            let mut output = vec![0; input.len()];
            target.get_bytes(output.as_mut_ptr().cast(), width * 4, region, 0);
            output
        }
    }

    fn scaled_bounds(x: f32, y: f32, width: f32, height: f32) -> Bounds<ScaledPixels> {
        Bounds::new(
            point(ScaledPixels(x), ScaledPixels(y)),
            size(ScaledPixels(width), ScaledPixels(height)),
        )
    }

    #[test]
    fn backdrop_filters_only_the_clipped_region_at_its_source_position() {
        let mut harness = Harness::new();
        let target = harness.target(256, 192);
        let mut input = vec![0u8; 256 * 192 * 4];
        for y in 0..192 {
            for x in 0..256 {
                let stripe = if x % 2 == 0 { 0 } else { 128 };
                let color = if x < 128 {
                    [stripe, 32, 64, 255]
                } else {
                    [stripe, 64, 32, 255]
                };
                input[(y * 256 + x) * 4..(y * 256 + x + 1) * 4].copy_from_slice(&color);
            }
        }
        for (radius, scratch_size) in [(0.0, (28, 24)), (4.0, (56, 52))] {
            for x in [80, 176] {
                let filter = BackdropFilter {
                    bounds: scaled_bounds(x as f32, 64., 48., 40.),
                    content_mask: ContentMask {
                        bounds: scaled_bounds((x + 8) as f32, 72., 28., 24.),
                    },
                    radius: ScaledPixels(radius),
                    opacity: 1.,
                    alpha_limit: 0.5,
                    ..Default::default()
                };
                let output = harness.filter(&target, &input, &filter);
                let scratch = harness.renderer.scratch.as_ref().unwrap();
                assert_eq!(
                    (
                        scratch.size.snapshot.width.0,
                        scratch.size.snapshot.height.0
                    ),
                    scratch_size,
                    "scratch textures fit the clipped output and blur halo"
                );
                for y in 0..192 {
                    for px in 0..256 {
                        if !(x + 8..x + 36).contains(&px) || !(72..96).contains(&y) {
                            let offset = (y * 256 + px) * 4;
                            assert_eq!(output[offset..offset + 4], input[offset..offset + 4]);
                        }
                    }
                }
                let offset = (80 * 256 + x + 20) * 4;
                let expected = if x < 128 { [16, 32] } else { [32, 16] };
                assert_eq!(
                    &output[offset + 1..offset + 4],
                    &[expected[0], expected[1], 128]
                );
                if radius > 0.0 {
                    assert!((28..=36).contains(&output[offset]), "stripes must soften");
                } else {
                    assert_eq!(output[offset], 0, "sampling without blur keeps position");
                }
            }
        }
    }

    #[test]
    fn backdrop_blur_keeps_alpha_and_respects_rounded_clipping() {
        let mut harness = Harness::new();
        let target = harness.target(96, 96);
        let mut input = vec![0u8; 96 * 96 * 4];
        for y in 0..96 {
            for x in 0..96 {
                let value = if x % 2 == 0 { 0 } else { 128 };
                input[(y * 96 + x) * 4..(y * 96 + x + 1) * 4]
                    .copy_from_slice(&[value, value, value, 128]);
            }
        }
        let bounds = scaled_bounds(16., 16., 64., 64.);
        let filter = BackdropFilter {
            bounds,
            content_mask: ContentMask {
                bounds: Bounds::new(bounds.origin, size(ScaledPixels(48.), ScaledPixels(64.))),
            },
            corner_radii: Corners::all(ScaledPixels(16.)),
            radius: ScaledPixels(8.),
            opacity: 1.,
            ..Default::default()
        };
        let output = harness.filter(&target, &input, &filter);
        let pixel = |x, y| &output[(y * 96 + x) * 4..(y * 96 + x + 1) * 4];
        assert!(
            (56..=72).contains(&pixel(40, 40)[0]),
            "alternating input blurs to its mean, got {:?}",
            pixel(40, 40)
        );
        assert!(
            pixel(40, 40)[0].abs_diff(pixel(41, 40)[0]) <= 3,
            "sharp stripes disappear"
        );
        assert_eq!(pixel(40, 40)[3], 128, "blur keeps destination alpha");
        for (x, y) in [(4, 40), (16, 16), (70, 40)] {
            assert_eq!(
                pixel(x, y),
                &input[(y * 96 + x) * 4..(y * 96 + x + 1) * 4],
                "outside, rounded-corner, and masked pixels stay unchanged"
            );
        }

        // Reused scratch textures capture the new target, not a previous frame.
        let constant = [16u8, 32, 64, 128].repeat(96 * 96);
        let output = harness.filter(&target, &constant, &filter);
        assert_eq!(output, constant, "a constant backdrop stays constant");
    }

    #[test]
    fn backdrop_small_positive_radius_preserves_alternating_columns() {
        let mut harness = Harness::new();
        let target = harness.target(64, 64);
        let mut input = vec![0u8; 64 * 64 * 4];
        for (index, pixel) in input.chunks_exact_mut(4).enumerate() {
            let value = if index % 64 % 2 == 0 { 0 } else { 255 };
            pixel.copy_from_slice(&[value, value, value, 255]);
        }
        let bounds = scaled_bounds(0., 0., 64., 64.);
        let mut filter = BackdropFilter {
            bounds,
            content_mask: ContentMask { bounds },
            opacity: 1.,
            ..Default::default()
        };
        let unchanged = harness.filter(&target, &input, &filter);
        assert_eq!(unchanged, input);

        filter.radius = ScaledPixels(0.01);
        let small = harness.filter(&target, &input, &filter);
        for x in 16..48 {
            let index = (32 * 64 + x) * 4;
            assert!(
                small[index].abs_diff(input[index]) <= 2,
                "small sigma changed column {x}"
            );
        }

        filter.radius = ScaledPixels(4.);
        let blurred = harness.filter(&target, &input, &filter);
        for x in 16..48 {
            let index = (32 * 64 + x) * 4;
            assert!(
                (100..=155).contains(&blurred[index]),
                "large sigma left column {x} sharp"
            );
        }
    }

    #[test]
    fn backdrop_blur_downsample_switches_without_aliasing_or_blur_jump() {
        const WIDTH: usize = 256;
        const HEIGHT: usize = 32;
        let mut harness = Harness::new();
        let target = harness.target(WIDTH as u64, HEIGHT as u64);
        let bounds = scaled_bounds(0., 0., WIDTH as f32, HEIGHT as f32);
        let mut filter = BackdropFilter {
            bounds,
            content_mask: ContentMask { bounds },
            opacity: 1.,
            ..Default::default()
        };
        let pattern = |frequency: f32, amplitude: f32| {
            let mut input = vec![0u8; WIDTH * HEIGHT * 4];
            for (index, pixel) in input.chunks_exact_mut(4).enumerate() {
                let x = index % WIDTH;
                let value = (127.
                    + amplitude * (std::f32::consts::TAU * frequency * x as f32).cos())
                .round() as u8;
                pixel.copy_from_slice(&[value, value, value, 255]);
            }
            input
        };
        fn interior(pixels: &[u8]) -> impl Iterator<Item = u8> + '_ {
            const WIDTH: usize = 256;
            (8..24).flat_map(move |y| (64..192).map(move |x| pixels[(y * WIDTH + x) * 4]))
        }

        for (threshold, near_nyquist) in [(5., 0.24), (10., 0.12)] {
            for frequency in [0.2, 0.4, near_nyquist] {
                let input = pattern(frequency, 100.);
                for radius in [threshold - 0.0001, threshold] {
                    filter.radius = ScaledPixels(radius);
                    let output = harness.filter(&target, &input, &filter);
                    let worst_residual = interior(&output)
                        .map(|value| value.abs_diff(127))
                        .max()
                        .unwrap();
                    eprintln!(
                        "sigma {radius}, frequency {frequency}: worst residual {worst_residual} LSB"
                    );
                    assert!(
                        worst_residual <= 2,
                        "sigma {radius}, frequency {frequency}: worst residual {worst_residual} LSB"
                    );
                }
            }

            let input = pattern(0.02, 60.);
            filter.radius = ScaledPixels(threshold - 0.0001);
            let below = harness.filter(&target, &input, &filter);
            filter.radius = ScaledPixels(threshold);
            let at = harness.filter(&target, &input, &filter);
            let worst_difference = interior(&below)
                .zip(interior(&at))
                .map(|(left, right)| left.abs_diff(right))
                .max()
                .unwrap();
            eprintln!("sigma {threshold}: worst smooth-pattern difference {worst_difference} LSB");
            assert!(
                worst_difference <= 2,
                "sigma {threshold}: worst smooth-pattern difference {worst_difference} LSB"
            );
        }
    }

    #[test]
    fn backdrop_tone_keeps_alpha_and_applies_once() {
        let mut harness = Harness::new();
        let target = harness.target(16, 16);
        let samples = [
            [0, 0, 0, 255],
            [64, 64, 64, 255],
            [255, 255, 255, 255],
            [128, 128, 128, 128],
            [0, 0, 0, 0],
        ];
        let mut input = vec![0u8; 16 * 16 * 4];
        for (index, pixel) in input.chunks_exact_mut(4).enumerate() {
            pixel.copy_from_slice(&samples[index % samples.len()]);
        }
        let bounds = scaled_bounds(0., 0., 16., 16.);
        let filter = BackdropFilter {
            bounds,
            content_mask: ContentMask { bounds },
            opacity: 1.,
            tone: rgba(0x202020b3),
            ..Default::default()
        };
        let once = harness.filter(&target, &input, &filter);
        let pixel = |index: usize| &once[index * 4..(index + 1) * 4];
        for channel in 0..3 {
            assert!((21..=24).contains(&pixel(0)[channel]));
            assert!((62..=66).contains(&pixel(1)[channel]));
            assert!((97..=100).contains(&pixel(2)[channel]));
            assert!((48..=51).contains(&pixel(3)[channel]));
            assert_eq!(pixel(4)[channel], 0);
        }
        assert_eq!(
            [
                pixel(0)[3],
                pixel(1)[3],
                pixel(2)[3],
                pixel(3)[3],
                pixel(4)[3]
            ],
            [255, 255, 255, 128, 0]
        );

        let twice = harness.filter(&target, &once, &filter);
        assert_eq!(twice, once, "the same tone does not flatten color again");
    }

    #[test]
    fn backdrop_alpha_limit_reveals_the_backing_once() {
        let mut harness = Harness::new();
        let target = harness.target(32, 32);
        let mut input = [64u8, 32, 16, 255].repeat(32 * 32);
        input[(16 * 32 + 12) * 4..(16 * 32 + 13) * 4].copy_from_slice(&[16, 8, 4, 32]);
        input[(16 * 32 + 13) * 4..(16 * 32 + 14) * 4].fill(0);
        let bounds = scaled_bounds(4., 4., 24., 24.);
        let mut filter = BackdropFilter {
            bounds,
            content_mask: ContentMask {
                bounds: Bounds::new(bounds.origin, size(ScaledPixels(20.), ScaledPixels(24.))),
            },
            corner_radii: Corners::all(ScaledPixels(6.)),
            opacity: 1.,
            alpha_limit: 0.25,
            ..Default::default()
        };
        let pixel = |bytes: &[u8], x: usize, y: usize| -> [u8; 4] {
            bytes[(y * 32 + x) * 4..(y * 32 + x + 1) * 4]
                .try_into()
                .unwrap()
        };

        let once = harness.filter(&target, &input, &filter);
        assert_eq!(
            pixel(&once, 16, 16),
            [16, 8, 4, 64],
            "dense content admits the native backing"
        );
        for (x, y) in [(12, 16), (13, 16), (0, 16), (4, 4), (26, 16)] {
            assert_eq!(
                pixel(&once, x, y),
                pixel(&input, x, y),
                "clear, thin, rounded, and masked pixels stay unchanged"
            );
        }

        // Rounded edges have partial coverage, like element opacity below. Fully covered
        // interior pixels are at the limit after the first application.
        let twice = harness.filter(&target, &once, &filter);
        for y in 10..22 {
            for x in 10..22 {
                assert_eq!(
                    pixel(&twice, x, y),
                    pixel(&once, x, y),
                    "nested filters do not attenuate the same interior again"
                );
            }
        }

        filter.opacity = 0.5;
        let partial = harness.filter(&target, &input, &filter);
        assert_eq!(
            pixel(&partial, 16, 16),
            [40, 20, 10, 159],
            "element opacity interpolates premultiplied color"
        );
    }
}
