use std::slice;

use anyhow::{Context as _, Result};
use gpui::{
    BackdropFilter, BackdropPass, BackdropScratchSize, BackdropUniforms, DevicePixels, Scene, Size,
};
use windows::Win32::{
    Foundation::RECT,
    Graphics::{Direct3D::*, Direct3D11::*, Dxgi::Common::*},
};

use super::{
    RENDER_TARGET_FORMAT, create_constant_buffer, create_fragment_shader, create_vertex_shader,
    shader_resources::{RawShaderBytes, ShaderModule, ShaderTarget},
    update_buffer,
};

/// Renders backdrop filters into the render target a scene draws to.
pub(super) struct BackdropRenderer {
    vertex: ID3D11VertexShader,
    fragment: ID3D11PixelShader,
    blend_state: ID3D11BlendState,
    rasterizer_state: ID3D11RasterizerState,
    sampler: Option<ID3D11SamplerState>,
    uniforms: Option<ID3D11Buffer>,
    scratch: Option<Scratch>,
}

/// Textures reused by every filter in a frame. They grow to the largest snapshot and are
/// released when a frame has no filters.
struct Scratch {
    size: BackdropScratchSize,
    snapshot: ScratchTexture,
    horizontal: ScratchTexture,
    vertical: ScratchTexture,
}

struct ScratchTexture {
    texture: ID3D11Texture2D,
    render_target_view: Option<ID3D11RenderTargetView>,
    shader_resource_view: Option<ID3D11ShaderResourceView>,
}

impl BackdropRenderer {
    pub(super) fn new(device: &ID3D11Device) -> Result<Self> {
        let vertex = {
            let raw_shader = RawShaderBytes::new(ShaderModule::Backdrop, ShaderTarget::Vertex)?;
            create_vertex_shader(device, raw_shader.as_bytes())?
        };
        let fragment = {
            let raw_shader = RawShaderBytes::new(ShaderModule::Backdrop, ShaderTarget::Fragment)?;
            create_fragment_shader(device, raw_shader.as_bytes())?
        };

        // The composite pass replaces premultiplied color and alpha.
        let mut blend_desc = D3D11_BLEND_DESC::default();
        blend_desc.RenderTarget[0].BlendEnable = false.into();
        blend_desc.RenderTarget[0].RenderTargetWriteMask = D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8;
        let rasterizer_desc = D3D11_RASTERIZER_DESC {
            FillMode: D3D11_FILL_SOLID,
            CullMode: D3D11_CULL_NONE,
            FrontCounterClockwise: false.into(),
            DepthBias: 0,
            DepthBiasClamp: 0.0,
            SlopeScaledDepthBias: 0.0,
            DepthClipEnable: true.into(),
            ScissorEnable: true.into(),
            MultisampleEnable: false.into(),
            AntialiasedLineEnable: false.into(),
        };
        let sampler_desc = D3D11_SAMPLER_DESC {
            Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
            AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
            MipLODBias: 0.0,
            MaxAnisotropy: 1,
            ComparisonFunc: D3D11_COMPARISON_ALWAYS,
            BorderColor: [0.0; 4],
            MinLOD: 0.0,
            MaxLOD: D3D11_FLOAT32_MAX,
        };
        let (blend_state, rasterizer_state, sampler) = unsafe {
            let mut blend_state = None;
            device.CreateBlendState(&blend_desc, Some(&mut blend_state))?;
            let mut rasterizer_state = None;
            device.CreateRasterizerState(&rasterizer_desc, Some(&mut rasterizer_state))?;
            let mut sampler = None;
            device.CreateSamplerState(&sampler_desc, Some(&mut sampler))?;
            (blend_state, rasterizer_state, sampler)
        };

        Ok(Self {
            vertex,
            fragment,
            blend_state: blend_state.context("creating backdrop blend state")?,
            rasterizer_state: rasterizer_state.context("creating backdrop rasterizer state")?,
            sampler,
            uniforms: create_constant_buffer::<BackdropUniforms>(device)?,
            scratch: None,
        })
    }

    /// Sizes the scratch textures for the filters in `scene`.
    pub(super) fn prepare(
        &mut self,
        device: &ID3D11Device,
        scene: &Scene,
        viewport: Size<DevicePixels>,
    ) -> Result<()> {
        let Some(required) = scene.backdrop_scratch_size(viewport) else {
            self.scratch = None;
            return Ok(());
        };
        if let Some(scratch) = &self.scratch
            && scratch.size.snapshot.width >= required.snapshot.width
            && scratch.size.snapshot.height >= required.snapshot.height
            && scratch.size.blur.width >= required.blur.width
            && scratch.size.blur.height >= required.blur.height
        {
            return Ok(());
        }
        let size = self
            .scratch
            .as_ref()
            .map_or(required, |scratch| BackdropScratchSize {
                snapshot: scratch.size.snapshot.max(&required.snapshot),
                blur: scratch.size.blur.max(&required.blur),
            });
        let (width, height) = (size.snapshot.width.0 as u32, size.snapshot.height.0 as u32);
        let (blur_width, blur_height) = (size.blur.width.0 as u32, size.blur.height.0 as u32);
        self.scratch = Some(Scratch {
            size,
            snapshot: ScratchTexture::new(device, width, height)?,
            horizontal: ScratchTexture::new(device, blur_width, blur_height)?,
            vertical: ScratchTexture::new(device, blur_width, blur_height)?,
        });
        Ok(())
    }

    /// Draws `filter` into `target`, which is bound as the render target with `viewport_state`
    /// and stays bound afterwards.
    pub(super) fn draw(
        &self,
        device_context: &ID3D11DeviceContext,
        target: &ID3D11Texture2D,
        target_view: &Option<ID3D11RenderTargetView>,
        viewport_state: &D3D11_VIEWPORT,
        filter: &BackdropFilter,
    ) -> Result<()> {
        let viewport = Size {
            width: DevicePixels(viewport_state.Width as i32),
            height: DevicePixels(viewport_state.Height as i32),
        };
        let (Some(scratch), Some(snapshot)) = (&self.scratch, filter.snapshot_bounds(viewport))
        else {
            return Ok(());
        };
        let uniforms = self
            .uniforms
            .as_ref()
            .context("backdrop uniforms missing")?;

        // A null rasterizer state is the default state and reads as an error.
        let previous_rasterizer_state = unsafe { device_context.RSGetState() }.ok();
        unsafe {
            device_context.CopySubresourceRegion(
                &scratch.snapshot.texture,
                0,
                0,
                0,
                0,
                target,
                0,
                Some(&D3D11_BOX {
                    left: snapshot.origin.x.0 as u32,
                    top: snapshot.origin.y.0 as u32,
                    front: 0,
                    right: snapshot.right().0 as u32,
                    bottom: snapshot.bottom().0 as u32,
                    back: 1,
                }),
            );
            device_context.RSSetState(&self.rasterizer_state);
            device_context.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            device_context.VSSetShader(&self.vertex, None);
            device_context.PSSetShader(&self.fragment, None);
            device_context.OMSetBlendState(&self.blend_state, None, 0xFFFFFFFF);
            device_context.PSSetConstantBuffers(2, Some(slice::from_ref(&self.uniforms)));
            device_context.PSSetSamplers(1, Some(slice::from_ref(&self.sampler)));
        }

        let result = filter.passes().iter().try_for_each(|&pass| {
            let (render_target_view, pass_viewport, source) = match pass {
                BackdropPass::Horizontal => (
                    &scratch.horizontal.render_target_view,
                    scratch.horizontal.viewport_state(),
                    &scratch.snapshot,
                ),
                BackdropPass::Vertical => (
                    &scratch.vertical.render_target_view,
                    scratch.vertical.viewport_state(),
                    &scratch.horizontal,
                ),
                BackdropPass::Composite if filter.radius.0 > 0.0 => {
                    (target_view, *viewport_state, &scratch.vertical)
                }
                BackdropPass::Composite => (target_view, *viewport_state, &scratch.snapshot),
            };
            let Some(scissor) = filter.pass_scissor(pass, snapshot, viewport) else {
                return Ok(());
            };
            update_buffer(
                device_context,
                uniforms,
                &[filter.uniforms(pass, snapshot, scratch.size, viewport)],
            )?;
            unsafe {
                device_context.OMSetRenderTargets(Some(slice::from_ref(render_target_view)), None);
                device_context.RSSetViewports(Some(&[pass_viewport]));
                device_context.RSSetScissorRects(Some(&[RECT {
                    left: scissor.origin.x.0,
                    top: scissor.origin.y.0,
                    right: scissor.right().0,
                    bottom: scissor.bottom().0,
                }]));
                device_context.PSSetShaderResources(
                    2,
                    Some(&[
                        source.shader_resource_view.clone(),
                        scratch.snapshot.shader_resource_view.clone(),
                    ]),
                );
                device_context.Draw(3, 0);
                // A texture read here is a render target of a later pass.
                device_context.PSSetShaderResources(2, Some(&[None, None]));
            }
            anyhow::Ok(())
        });

        unsafe {
            device_context.OMSetRenderTargets(Some(slice::from_ref(target_view)), None);
            device_context.RSSetViewports(Some(slice::from_ref(viewport_state)));
            device_context.RSSetState(previous_rasterizer_state.as_ref());
        }
        result
    }
}

impl ScratchTexture {
    fn new(device: &ID3D11Device, width: u32, height: u32) -> Result<Self> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: RENDER_TARGET_FORMAT,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        unsafe {
            let mut texture = None;
            device.CreateTexture2D(&desc, None, Some(&mut texture))?;
            let texture = texture.context("creating backdrop scratch texture")?;
            let mut render_target_view = None;
            device.CreateRenderTargetView(&texture, None, Some(&mut render_target_view))?;
            let mut shader_resource_view = None;
            device.CreateShaderResourceView(&texture, None, Some(&mut shader_resource_view))?;
            Ok(Self {
                texture,
                render_target_view,
                shader_resource_view,
            })
        }
    }

    fn viewport_state(&self) -> D3D11_VIEWPORT {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { self.texture.GetDesc(&mut desc) };
        D3D11_VIEWPORT {
            TopLeftX: 0.0,
            TopLeftY: 0.0,
            Width: desc.Width as f32,
            Height: desc.Height as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        }
    }
}
