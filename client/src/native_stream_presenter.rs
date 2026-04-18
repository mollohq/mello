#![cfg(target_os = "windows")]

use std::ffi::c_void;

use windows::core::{s, w};
use windows::Win32::Foundation::{HANDLE, HMODULE, HWND};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{
    ID3DBlob, ID3DInclude, D3D_DRIVER_TYPE_HARDWARE, D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST,
    D3D_SRV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDeviceAndSwapChain, ID3D11Buffer, ID3D11ClassLinkage, ID3D11DepthStencilView,
    ID3D11Device, ID3D11DeviceContext, ID3D11PixelShader, ID3D11RenderTargetView,
    ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11VertexShader, D3D11_BIND_CONSTANT_BUFFER,
    D3D11_BUFFER_DESC, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
    D3D11_SHADER_RESOURCE_VIEW_DESC, D3D11_TEX2D_SRV, D3D11_USAGE_DEFAULT, D3D11_VIEWPORT,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_FORMAT_R8_UNORM, DXGI_MODE_DESC, DXGI_RATIONAL,
    DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGISwapChain, DXGI_ERROR_WAS_STILL_DRAWING, DXGI_PRESENT_DO_NOT_WAIT, DXGI_SWAP_CHAIN_DESC,
    DXGI_SWAP_CHAIN_FLAG, DXGI_SWAP_EFFECT_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, SetWindowPos, ShowWindow, SWP_NOACTIVATE, SWP_SHOWWINDOW,
    SW_HIDE, WINDOW_EX_STYLE, WS_CHILD, WS_CLIPCHILDREN, WS_CLIPSIBLINGS,
};

const NATIVE_FMT_RGBA8: u32 = 1;
const NATIVE_FMT_R8_NV12_LAYOUT: u32 = 2;

const R8_NV12_VS_HLSL: &str = r#"
struct VSOut {
    float4 pos : SV_Position;
    float2 uv  : TEXCOORD0;
};

VSOut vs_main(uint id : SV_VertexID) {
    float2 pos = float2((id == 2) ? 3.0 : -1.0, (id == 1) ? 3.0 : -1.0);
    VSOut o;
    o.pos = float4(pos, 0.0, 1.0);
    o.uv = float2((pos.x + 1.0) * 0.5, 1.0 - ((pos.y + 1.0) * 0.5));
    return o;
}
"#;

const R8_NV12_PS_HLSL: &str = r#"
Texture2D<float> nv12_tex : register(t0);

cbuffer Cb : register(b0) {
    uint vid_w;
    uint vid_h;
    uint uv_y;
    uint _pad;
};

float4 ps_main(float4 pos : SV_Position, float2 uv : TEXCOORD0) : SV_Target {
    uint x = min((uint)(uv.x * vid_w), vid_w - 1);
    uint y = min((uint)(uv.y * vid_h), vid_h - 1);

    float y_raw = nv12_tex.Load(int3(x, y, 0)).r * 255.0;

    uint uv_row = uv_y + y / 2;
    uint uv_col = x & ~1u;
    float u_raw = nv12_tex.Load(int3(uv_col, uv_row, 0)).r * 255.0;
    float v_raw = nv12_tex.Load(int3(uv_col + 1, uv_row, 0)).r * 255.0;

    float c = y_raw - 16.0;
    float d = u_raw - 128.0;
    float e = v_raw - 128.0;

    // BT.709 studio-swing
    float r = (298.0 * c + 459.0 * e + 128.0) / 256.0 / 255.0;
    float g = (298.0 * c -  55.0 * d - 136.0 * e + 128.0) / 256.0 / 255.0;
    float b = (298.0 * c + 541.0 * d + 128.0) / 256.0 / 255.0;

    return float4(saturate(r), saturate(g), saturate(b), 1.0);
}
"#;

#[repr(C)]
#[derive(Clone, Copy)]
struct Nv12LayoutConstants {
    vid_w: u32,
    vid_h: u32,
    uv_y: u32,
    _pad: u32,
}

struct R8Nv12Pipeline {
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    cb: ID3D11Buffer,
}

pub struct NativeStreamPresenter {
    child_hwnd: HWND,
    device: ID3D11Device,
    swap_chain: IDXGISwapChain,
    context: ID3D11DeviceContext,
    back_buffer: Option<ID3D11Texture2D>,
    back_rtv: Option<ID3D11RenderTargetView>,
    backbuffer_w: u32,
    backbuffer_h: u32,
    shared_source: Option<ID3D11Texture2D>,
    shared_source_handle: usize,
    shared_source_format: u32,
    shared_source_srv: Option<ID3D11ShaderResourceView>,
    r8_nv12_pipeline: Option<R8Nv12Pipeline>,
    viewport_x: i32,
    viewport_y: i32,
    viewport_w: i32,
    viewport_h: i32,
    viewport_visible: bool,
}

impl NativeStreamPresenter {
    pub fn new(parent_hwnd: HWND) -> Result<Self, String> {
        unsafe {
            let child_hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!(""),
                WS_CHILD | WS_CLIPSIBLINGS | WS_CLIPCHILDREN,
                0,
                0,
                2,
                2,
                Some(parent_hwnd),
                None,
                None,
                None,
            )
            .map_err(|e| format!("CreateWindowExW failed: {}", e))?;

            let mut swap_desc = DXGI_SWAP_CHAIN_DESC::default();
            swap_desc.BufferDesc = DXGI_MODE_DESC {
                Width: 2,
                Height: 2,
                RefreshRate: DXGI_RATIONAL {
                    Numerator: 0,
                    Denominator: 1,
                },
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                ..Default::default()
            };
            swap_desc.SampleDesc = DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            };
            swap_desc.BufferUsage = DXGI_USAGE_RENDER_TARGET_OUTPUT;
            swap_desc.BufferCount = 2;
            swap_desc.OutputWindow = child_hwnd;
            swap_desc.Windowed = true.into();
            swap_desc.SwapEffect = DXGI_SWAP_EFFECT_DISCARD;

            let mut swap_chain = None;
            let mut device = None;
            let mut context = None;
            let mut feature_level = Default::default();

            D3D11CreateDeviceAndSwapChain(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&swap_desc),
                Some(&mut swap_chain),
                Some(&mut device),
                Some(&mut feature_level),
                Some(&mut context),
            )
            .map_err(|e| format!("D3D11CreateDeviceAndSwapChain failed: {}", e))?;

            let _device = device.ok_or_else(|| "D3D11 device missing".to_string())?;
            let swap_chain = swap_chain.ok_or_else(|| "DXGI swapchain missing".to_string())?;
            let context = context.ok_or_else(|| "D3D11 context missing".to_string())?;

            Ok(Self {
                child_hwnd,
                device: _device,
                swap_chain,
                context,
                back_buffer: None,
                back_rtv: None,
                backbuffer_w: 0,
                backbuffer_h: 0,
                shared_source: None,
                shared_source_handle: 0,
                shared_source_format: 0,
                shared_source_srv: None,
                r8_nv12_pipeline: None,
                viewport_x: 0,
                viewport_y: 0,
                viewport_w: 0,
                viewport_h: 0,
                viewport_visible: false,
            })
        }
    }

    pub fn update_viewport(&mut self, x: i32, y: i32, width: i32, height: i32, visible: bool) {
        let geometry_changed = self.viewport_x != x
            || self.viewport_y != y
            || self.viewport_w != width
            || self.viewport_h != height;
        let visibility_changed = self.viewport_visible != visible;
        if !geometry_changed && !visibility_changed {
            return;
        }
        unsafe {
            if visible && width > 0 && height > 0 {
                let _ = SetWindowPos(
                    self.child_hwnd,
                    None,
                    x,
                    y,
                    width,
                    height,
                    SWP_NOACTIVATE | SWP_SHOWWINDOW,
                );
            } else {
                let _hidden = ShowWindow(self.child_hwnd, SW_HIDE).as_bool();
            }
        }
        self.viewport_x = x;
        self.viewport_y = y;
        self.viewport_w = width;
        self.viewport_h = height;
        self.viewport_visible = visible;
    }

    pub fn render_frame(&mut self, width: u32, height: u32, rgba: &[u8]) -> Result<(), String> {
        let expected_len = (width as usize) * (height as usize) * 4;
        if rgba.len() != expected_len {
            return Ok(());
        }

        self.ensure_backbuffer(width, height)?;

        let Some(back_buffer) = self.back_buffer.as_ref() else {
            return Ok(());
        };

        unsafe {
            self.context.UpdateSubresource(
                back_buffer,
                0,
                None,
                rgba.as_ptr() as *const c_void,
                width * 4,
                0,
            );

            let present = self.swap_chain.Present(0, DXGI_PRESENT_DO_NOT_WAIT);
            if let Err(e) = present.ok() {
                if e.code() != DXGI_ERROR_WAS_STILL_DRAWING {
                    return Err(format!("swapchain present failed: {}", e));
                }
            }
        }

        Ok(())
    }

    pub fn render_shared_frame(
        &mut self,
        width: u32,
        height: u32,
        shared_handle: usize,
        format: u32,
        uv_y_offset: u32,
    ) -> Result<(), String> {
        if shared_handle == 0 {
            return Ok(());
        }
        self.ensure_backbuffer(width, height)?;
        self.ensure_shared_source(shared_handle, format)?;

        unsafe {
            let Some(back_buffer) = self.back_buffer.as_ref() else {
                return Ok(());
            };
            let Some(shared_source) = self.shared_source.as_ref() else {
                return Ok(());
            };

            match format {
                NATIVE_FMT_RGBA8 => {
                    self.context.CopyResource(back_buffer, shared_source);
                }
                NATIVE_FMT_R8_NV12_LAYOUT => {
                    self.render_r8_nv12(width, height, uv_y_offset)?;
                }
                _ => return Ok(()),
            }

            self.present_nonblocking()?;
        }
        Ok(())
    }

    fn ensure_shared_source(&mut self, shared_handle: usize, format: u32) -> Result<(), String> {
        if self.shared_source_handle == shared_handle
            && self.shared_source_format == format
            && self.shared_source.is_some()
        {
            return Ok(());
        }

        self.shared_source = None;
        self.shared_source_srv = None;
        unsafe {
            let mut opened = None;
            self.device
                .OpenSharedResource(
                    HANDLE(shared_handle as *mut c_void),
                    &mut opened as *mut Option<ID3D11Texture2D>,
                )
                .map_err(|e| format!("OpenSharedResource failed: {}", e))?;
            self.shared_source = opened;
        }
        self.shared_source_handle = shared_handle;
        self.shared_source_format = format;

        if format == NATIVE_FMT_R8_NV12_LAYOUT {
            let Some(shared_source) = self.shared_source.as_ref() else {
                return Ok(());
            };
            let mut srv = None;
            let mut srv_desc = D3D11_SHADER_RESOURCE_VIEW_DESC::default();
            srv_desc.Format = DXGI_FORMAT_R8_UNORM;
            srv_desc.ViewDimension = D3D_SRV_DIMENSION_TEXTURE2D;
            srv_desc.Anonymous.Texture2D = D3D11_TEX2D_SRV {
                MostDetailedMip: 0,
                MipLevels: 1,
            };
            unsafe {
                self.device
                    .CreateShaderResourceView(shared_source, Some(&srv_desc), Some(&mut srv))
                    .map_err(|e| format!("CreateShaderResourceView failed: {}", e))?;
            }
            self.shared_source_srv = srv;
        }

        Ok(())
    }

    fn ensure_r8_nv12_pipeline(&mut self) -> Result<(), String> {
        if self.r8_nv12_pipeline.is_some() {
            return Ok(());
        }

        unsafe {
            let vs_blob = compile_shader_blob(R8_NV12_VS_HLSL, s!("vs_main"), s!("vs_5_0"))?;
            let ps_blob = compile_shader_blob(R8_NV12_PS_HLSL, s!("ps_main"), s!("ps_5_0"))?;
            let vs_bytes = blob_bytes(&vs_blob);
            let ps_bytes = blob_bytes(&ps_blob);

            let mut vs = None;
            let mut ps = None;
            self.device
                .CreateVertexShader(vs_bytes, None::<&ID3D11ClassLinkage>, Some(&mut vs))
                .map_err(|e| format!("CreateVertexShader failed: {}", e))?;
            self.device
                .CreatePixelShader(ps_bytes, None::<&ID3D11ClassLinkage>, Some(&mut ps))
                .map_err(|e| format!("CreatePixelShader failed: {}", e))?;

            let cb_desc = D3D11_BUFFER_DESC {
                ByteWidth: std::mem::size_of::<Nv12LayoutConstants>() as u32,
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
                StructureByteStride: 0,
            };
            let mut cb = None;
            self.device
                .CreateBuffer(&cb_desc, None, Some(&mut cb))
                .map_err(|e| format!("CreateBuffer failed: {}", e))?;

            self.r8_nv12_pipeline = Some(R8Nv12Pipeline {
                vs: vs.ok_or_else(|| "Vertex shader missing".to_string())?,
                ps: ps.ok_or_else(|| "Pixel shader missing".to_string())?,
                cb: cb.ok_or_else(|| "Constant buffer missing".to_string())?,
            });
        }

        Ok(())
    }

    fn render_r8_nv12(&mut self, width: u32, height: u32, uv_y_offset: u32) -> Result<(), String> {
        self.ensure_r8_nv12_pipeline()?;
        let Some(pipeline) = self.r8_nv12_pipeline.as_ref() else {
            return Ok(());
        };
        let Some(back_rtv) = self.back_rtv.as_ref() else {
            return Ok(());
        };
        let Some(source_srv) = self.shared_source_srv.as_ref() else {
            return Ok(());
        };

        let cb_data = Nv12LayoutConstants {
            vid_w: width,
            vid_h: height,
            uv_y: uv_y_offset,
            _pad: 0,
        };
        unsafe {
            self.context.UpdateSubresource(
                &pipeline.cb,
                0,
                None,
                &cb_data as *const _ as *const c_void,
                0,
                0,
            );

            let rtvs = [Some(back_rtv.clone())];
            self.context
                .OMSetRenderTargets(Some(&rtvs), None::<&ID3D11DepthStencilView>);

            let viewport = D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: self.backbuffer_w as f32,
                Height: self.backbuffer_h as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            };
            self.context.RSSetViewports(Some(&[viewport]));
            self.context
                .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            self.context.VSSetShader(&pipeline.vs, None);
            self.context.PSSetShader(&pipeline.ps, None);

            let cbs = [Some(pipeline.cb.clone())];
            self.context.PSSetConstantBuffers(0, Some(&cbs));

            let srvs = [Some(source_srv.clone())];
            self.context.PSSetShaderResources(0, Some(&srvs));
            self.context.Draw(3, 0);

            let null_srvs: [Option<ID3D11ShaderResourceView>; 1] = [None];
            self.context.PSSetShaderResources(0, Some(&null_srvs));
        }

        Ok(())
    }

    fn present_nonblocking(&self) -> Result<(), String> {
        unsafe {
            let present = self.swap_chain.Present(0, DXGI_PRESENT_DO_NOT_WAIT);
            if let Err(e) = present.ok() {
                if e.code() != DXGI_ERROR_WAS_STILL_DRAWING {
                    return Err(format!("swapchain present failed: {}", e));
                }
            }
        }
        Ok(())
    }

    fn ensure_backbuffer(&mut self, width: u32, height: u32) -> Result<(), String> {
        if self.backbuffer_w == width && self.backbuffer_h == height && self.back_buffer.is_some() {
            return Ok(());
        }

        self.back_buffer = None;
        self.back_rtv = None;
        self.shared_source = None;
        self.shared_source_handle = 0;
        self.shared_source_format = 0;
        self.shared_source_srv = None;
        unsafe {
            self.swap_chain
                .ResizeBuffers(
                    0,
                    width,
                    height,
                    DXGI_FORMAT_R8G8B8A8_UNORM,
                    DXGI_SWAP_CHAIN_FLAG(0),
                )
                .map_err(|e| format!("swapchain ResizeBuffers failed: {}", e))?;

            let back_buffer = self
                .swap_chain
                .GetBuffer::<ID3D11Texture2D>(0)
                .map_err(|e| format!("swapchain GetBuffer failed: {}", e))?;
            let mut back_rtv = None;
            self.device
                .CreateRenderTargetView(&back_buffer, None, Some(&mut back_rtv))
                .map_err(|e| format!("CreateRenderTargetView failed: {}", e))?;

            self.back_buffer = Some(back_buffer);
            self.back_rtv = back_rtv;
        }
        self.backbuffer_w = width;
        self.backbuffer_h = height;
        Ok(())
    }
}

impl Drop for NativeStreamPresenter {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.child_hwnd);
        }
    }
}

unsafe fn compile_shader_blob(
    source: &str,
    entry: windows::core::PCSTR,
    target: windows::core::PCSTR,
) -> Result<ID3DBlob, String> {
    let mut shader = None;
    let mut errors = None;
    let compile_result = D3DCompile(
        source.as_ptr() as *const c_void,
        source.len(),
        s!("native_stream_presenter"),
        None,
        None::<&ID3DInclude>,
        entry,
        target,
        0,
        0,
        &mut shader,
        Some(&mut errors),
    );

    if let Err(e) = compile_result {
        if let Some(err_blob) = errors {
            let msg = String::from_utf8_lossy(blob_bytes(&err_blob)).to_string();
            return Err(format!("D3DCompile failed: {} ({})", e, msg.trim()));
        }
        return Err(format!("D3DCompile failed: {}", e));
    }
    shader.ok_or_else(|| "D3DCompile returned no shader blob".to_string())
}

unsafe fn blob_bytes<'a>(blob: &'a ID3DBlob) -> &'a [u8] {
    std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize())
}
