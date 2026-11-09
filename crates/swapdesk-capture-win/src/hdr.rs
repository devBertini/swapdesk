//! Displays in Advanced Color (HDR or automatic colour management): the desktop is composed in FP16 scRGB, and the
//! legacy `DuplicateOutput` hands it out as 8-bit BGRA by clamping the linear values to 0..1, without the SDR white
//! level. scRGB 1.0 is 80 nits while Windows draws SDR white at the user's SDR white level (320 nits = 4.0 on the
//! external display of the development PC), so every SDR colour came out four times too bright in linear light and
//! everything above sRGB ~138 clipped to white (measured against GDI on 2026-11-09; see `examples/output_color.rs`).
//!
//! The fix asks for FP16 with `IDXGIOutput5::DuplicateOutput1` and converts it here on the GPU: linear scRGB divided by
//! the SDR white level, clamped to 0..1 (HDR highlights above SDR white clip; the desktop is SDR content), sRGB-encoded
//! into the BGRA frame texture. Sources read on 2026-11-09: "Using DirectX with high dynamic range displays and Advanced
//! Color" (<https://learn.microsoft.com/en-us/windows/win32/direct3darticles/high-dynamic-range>: CCCS is scRGB FP16,
//! scRGB 1.0 is 80 nits, SDR content is scaled by `SdrWhiteLevelInNits / 80`),
//! `DISPLAYCONFIG_SDR_WHITE_LEVEL` (<https://learn.microsoft.com/en-us/windows/win32/api/wingdi/ns-wingdi-displayconfig_sdr_white_level>:
//! `SDRWhiteLevel` is a multiplier of 80 nits times 1000) and `IDXGIOutput5::DuplicateOutput1`
//! (<https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_5/nf-dxgi1_5-idxgioutput5-duplicateoutput1>).

use crate::errors::{self, Phase};
use swapdesk_platform::{ErrorKind, Result};
use windows::Win32::Devices::Display::{
    DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL, DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
    DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SDR_WHITE_LEVEL,
    DISPLAYCONFIG_SOURCE_DEVICE_NAME, DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QDC_ONLY_ACTIVE_PATHS,
    QueryDisplayConfig,
};
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST, ID3DBlob};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_SHADER_RESOURCE, D3D11_BUFFER_DESC, D3D11_SUBRESOURCE_DATA,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_VIEWPORT, ID3D11Buffer, ID3D11Device, ID3D11DeviceContext,
    ID3D11PixelShader, ID3D11RenderTargetView, ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11VertexShader,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_SAMPLE_DESC};
use windows::core::{PCSTR, s};

/// The format the composed desktop of an Advanced Color display comes in.
pub(crate) const FP16: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT = DXGI_FORMAT_R16G16B16A16_FLOAT;

/// One full-screen triangle; each pixel of the target reads the same pixel of the source (no filtering).
const SHADER: &str = r"
Texture2D<float4> src : register(t0);
cbuffer Scale : register(b0) { float inv_white; float3 unused; };
float4 vs(uint id : SV_VertexID) : SV_Position {
    float2 uv = float2((id << 1) & 2, id & 2);
    return float4(uv * float2(2, -2) + float2(-1, 1), 0, 1);
}
float4 ps(float4 pos : SV_Position) : SV_Target {
    float3 l = saturate(src.Load(int3(pos.xy, 0)).rgb * inv_white);
    float3 lo = l * 12.92;
    float3 hi = 1.055 * pow(l, 1.0 / 2.4) - 0.055;
    return float4(l <= 0.0031308 ? lo : hi, 1);
}
";

/// The CPU reference of the shader for one channel: scRGB linear value to 8-bit sRGB at an SDR white of `white`
/// (scRGB units, 1.0 = 80 nits).
pub fn scrgb_to_srgb8(linear: f32, white: f32) -> u8 {
    let l = (linear / white.max(f32::MIN_POSITIVE)).clamp(0.0, 1.0);
    let e = if l <= 0.003_130_8 { l * 12.92 } else { 1.055 * l.powf(1.0 / 2.4) - 0.055 };
    (e * 255.0).round().clamp(0.0, 255.0) as u8
}

fn wide(w: &[u16]) -> String {
    let end = w.iter().position(|&c| c == 0).unwrap_or(w.len());
    String::from_utf16_lossy(&w[..end])
}

/// The SDR white level of the display whose GDI name is `gdi_name` (`\\.\DISPLAY5`), in scRGB units (1.0 = 80 nits).
/// `None` when the display is not found or does not answer (then the frames are taken as they come).
pub(crate) fn sdr_white(gdi_name: &str) -> Option<f32> {
    let (mut np, mut nm) = (0u32, 0u32);
    // SAFETY: sized buffers for QueryDisplayConfig, then one request packet per query with its header filled in.
    unsafe {
        if GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm) != ERROR_SUCCESS {
            return None;
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
        if QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS, &mut np, paths.as_mut_ptr(), &mut nm, modes.as_mut_ptr(), None)
            != ERROR_SUCCESS
        {
            return None;
        }
        for p in &paths[..np as usize] {
            let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
                header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                    r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                    size: size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                    adapterId: p.sourceInfo.adapterId,
                    id: p.sourceInfo.id,
                },
                ..Default::default()
            };
            if DisplayConfigGetDeviceInfo(&mut source.header) != 0
                || !wide(&source.viewGdiDeviceName).eq_ignore_ascii_case(gdi_name)
            {
                continue;
            }
            let mut white = DISPLAYCONFIG_SDR_WHITE_LEVEL {
                header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                    r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL,
                    size: size_of::<DISPLAYCONFIG_SDR_WHITE_LEVEL>() as u32,
                    adapterId: p.targetInfo.adapterId,
                    id: p.targetInfo.id,
                },
                ..Default::default()
            };
            if DisplayConfigGetDeviceInfo(&mut white.header) != 0 || white.SDRWhiteLevel == 0 {
                return None;
            }
            return Some(white.SDRWhiteLevel as f32 / 1000.0);
        }
    }
    None
}

fn os(op: &'static str, e: &windows::core::Error) -> swapdesk_platform::PlatformError {
    errors::from_hresult(Phase::Frame, op, e.code().0, &e.message())
}

fn compile(entry: PCSTR, target: PCSTR) -> Result<Vec<u8>> {
    let mut code: Option<ID3DBlob> = None;
    let mut messages: Option<ID3DBlob> = None;
    // SAFETY: the source buffer and its length, NUL-terminated entry and target names, valid out pointers.
    let result = unsafe {
        D3DCompile(
            SHADER.as_ptr().cast(),
            SHADER.len(),
            s!("scrgb_to_srgb"),
            None,
            None,
            entry,
            target,
            0,
            0,
            &mut code,
            Some(&mut messages),
        )
    };
    let blob_bytes = |b: &ID3DBlob| {
        // SAFETY: a live blob's pointer and size describe its bytes.
        unsafe { std::slice::from_raw_parts(b.GetBufferPointer().cast::<u8>(), b.GetBufferSize()).to_vec() }
    };
    match (result, code) {
        (Ok(()), Some(c)) => Ok(blob_bytes(&c)),
        (r, _) => {
            let text =
                messages.as_ref().map(|m| String::from_utf8_lossy(&blob_bytes(m)).into_owned()).unwrap_or_default();
            Err(errors::error(ErrorKind::Os, "D3DCompile", r.err().map(|e| e.code().0), text))
        }
    }
}

/// The FP16 to BGRA conversion on one device: shaders, the white-level constant and a copy of the duplicated surface
/// (the duplication's own texture is copied first, so no view of it is ever made).
pub(crate) struct ScrgbToSrgb {
    device: ID3D11Device,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    scale: ID3D11Buffer,
    white: f32,
    source: Option<(ID3D11Texture2D, ID3D11ShaderResourceView, u32, u32)>,
}

impl ScrgbToSrgb {
    pub(crate) fn new(device: &ID3D11Device) -> Result<Self> {
        let vs_code = compile(s!("vs"), s!("vs_4_0"))?;
        let ps_code = compile(s!("ps"), s!("ps_4_0"))?;
        let (mut vs, mut ps, mut scale) = (None, None, None);
        let data = [1.0f32, 0.0, 0.0, 0.0];
        let desc = D3D11_BUFFER_DESC {
            ByteWidth: 16,
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            ..Default::default()
        };
        let init = D3D11_SUBRESOURCE_DATA { pSysMem: data.as_ptr().cast(), ..Default::default() };
        // SAFETY: compiled bytecode, a 16-byte constant buffer with its initial data, valid out pointers.
        unsafe {
            device.CreateVertexShader(&vs_code, None, Some(&mut vs)).map_err(|e| os("CreateVertexShader", &e))?;
            device.CreatePixelShader(&ps_code, None, Some(&mut ps)).map_err(|e| os("CreatePixelShader", &e))?;
            device.CreateBuffer(&desc, Some(&init), Some(&mut scale)).map_err(|e| os("CreateBuffer", &e))?;
        }
        let missing = |what| errors::error(ErrorKind::Os, what, None, "nothing returned");
        Ok(Self {
            device: device.clone(),
            vs: vs.ok_or_else(|| missing("CreateVertexShader"))?,
            ps: ps.ok_or_else(|| missing("CreatePixelShader"))?,
            scale: scale.ok_or_else(|| missing("CreateBuffer"))?,
            white: 1.0,
            source: None,
        })
    }

    fn source(&mut self, width: u32, height: u32) -> Result<(ID3D11Texture2D, ID3D11ShaderResourceView)> {
        if let Some((t, v, w, h)) = &self.source
            && (*w, *h) == (width, height)
        {
            return Ok((t.clone(), v.clone()));
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: FP16,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let (mut tex, mut view) = (None, None);
        // SAFETY: a valid description and out pointers; the view is made of the texture just created.
        unsafe {
            self.device.CreateTexture2D(&desc, None, Some(&mut tex)).map_err(|e| os("CreateTexture2D", &e))?;
            let t = tex.as_ref().ok_or_else(|| errors::error(ErrorKind::Os, "CreateTexture2D", None, "no texture"))?;
            self.device
                .CreateShaderResourceView(t, None, Some(&mut view))
                .map_err(|e| os("CreateShaderResourceView", &e))?;
        }
        let (Some(t), Some(v)) = (tex, view) else {
            return Err(errors::error(ErrorKind::Os, "CreateShaderResourceView", None, "no view"));
        };
        self.source = Some((t.clone(), v.clone(), width, height));
        Ok((t, v))
    }

    /// Writes `src` (FP16 scRGB, `width` x `height`) into `dst` (BGRA, same size, render-target bindable) as sRGB
    /// with `white` (scRGB units) mapped to 255.
    pub(crate) fn convert(
        &mut self,
        context: &ID3D11DeviceContext,
        src: &ID3D11Texture2D,
        dst: &ID3D11Texture2D,
        (width, height): (u32, u32),
        white: f32,
    ) -> Result<()> {
        let (copy, view) = self.source(width, height)?;
        let mut target: Option<ID3D11RenderTargetView> = None;
        // SAFETY: `dst` is a live render-target texture of this device.
        unsafe { self.device.CreateRenderTargetView(dst, None, Some(&mut target)) }
            .map_err(|e| os("CreateRenderTargetView", &e))?;
        let viewport =
            D3D11_VIEWPORT { Width: width as f32, Height: height as f32, MaxDepth: 1.0, ..Default::default() };
        // SAFETY: state changes and one draw on the device's own immediate context (multithread protected); the
        // copy has the source's size and format; every binding is undone after the draw so the frame texture is free
        // for the encoder.
        unsafe {
            context.CopyResource(&copy, src);
            if (white - self.white).abs() > f32::EPSILON {
                let data = [1.0 / white.max(f32::MIN_POSITIVE), 0.0, 0.0, 0.0];
                context.UpdateSubresource(&self.scale, 0, None, data.as_ptr().cast(), 0, 0);
                self.white = white;
            }
            context.IASetInputLayout(None);
            context.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            context.VSSetShader(&self.vs, None);
            context.PSSetShader(&self.ps, None);
            context.PSSetShaderResources(0, Some(&[Some(view)]));
            context.PSSetConstantBuffers(0, Some(&[Some(self.scale.clone())]));
            context.OMSetRenderTargets(Some(&[target]), None);
            context.RSSetViewports(Some(&[viewport]));
            context.Draw(3, 0);
            context.OMSetRenderTargets(None, None);
            context.PSSetShaderResources(0, Some(&[None]));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sdr_white_maps_to_255_and_srgb_values_come_back() {
        // An SDR pixel drawn at a white level of 4.0 (320 nits) is its sRGB value, linearised, times 4.
        let lin = |v: u8| {
            let c = f32::from(v) / 255.0;
            if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
        };
        for v in [0u8, 1, 10, 16, 50, 100, 128, 138, 200, 235, 255] {
            assert_eq!(scrgb_to_srgb8(lin(v) * 4.0, 4.0), v, "value {v} at white 4.0");
            assert_eq!(scrgb_to_srgb8(lin(v), 1.0), v, "value {v} at white 1.0");
        }
        // Above SDR white (an HDR highlight) and below black clip.
        assert_eq!(scrgb_to_srgb8(9.0, 4.0), 255);
        assert_eq!(scrgb_to_srgb8(-0.2, 4.0), 0);
        // Without the white level (what the legacy duplication does), sRGB 50 comes out as 100 (measured 100.7) and 140 as 255.
        assert_eq!(scrgb_to_srgb8(lin(50) * 4.0, 1.0), 100);
        assert_eq!(scrgb_to_srgb8(lin(140) * 4.0, 1.0), 255);
    }

    /// The shader against its CPU reference: an FP16 texture of known scRGB values (SDR colours drawn at a white of 4.0,
    /// an HDR highlight, a colour outside sRGB) through the GPU into BGRA, read back.
    #[test]
    fn the_gpu_conversion_matches_the_reference() {
        use half_free::to_f16;
        use windows::Win32::Foundation::HMODULE;
        use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
        use windows::Win32::Graphics::Direct3D11::{
            D3D11_BIND_RENDER_TARGET, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
            D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_USAGE_STAGING, D3D11CreateDevice,
        };
        use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;

        let lin = |v: u8| {
            let c = f32::from(v) / 255.0;
            if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
        };
        let white = 4.0f32;
        // (r, g, b) in scRGB, one pixel each, in a 16x1 texture.
        let mut pixels: Vec<[f32; 3]> = [0u8, 16, 50, 100, 128, 138, 200, 235, 255]
            .iter()
            .map(|&v| [lin(v) * white, lin(v / 2) * white, lin(255 - v) * white])
            .collect();
        pixels.push([9.0, 2.0, 0.5]); // highlight above SDR white
        pixels.push([-0.1, 1.2, 0.3]); // outside the sRGB gamut
        pixels.resize(16, [0.0; 3]);
        let (w, h) = (16u32, 1u32);
        let data: Vec<u16> =
            pixels.iter().flat_map(|p| [to_f16(p[0]), to_f16(p[1]), to_f16(p[2]), to_f16(1.0)]).collect();

        let (mut device, mut context) = (None, None);
        // SAFETY: a hardware device with valid out pointers; textures with their descriptions and initial data; a
        // staging copy mapped for reading and unmapped.
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .expect("device");
            let (device, context): (ID3D11Device, ID3D11DeviceContext) = (device.unwrap(), context.unwrap());
            let desc = |format, bind: u32, staging: bool| D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: format,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: if staging { D3D11_USAGE_STAGING } else { D3D11_USAGE_DEFAULT },
                BindFlags: bind,
                CPUAccessFlags: if staging { D3D11_CPU_ACCESS_READ.0 as u32 } else { 0 },
                MiscFlags: 0,
            };
            let init =
                D3D11_SUBRESOURCE_DATA { pSysMem: data.as_ptr().cast(), SysMemPitch: w * 8, ..Default::default() };
            let (mut src, mut dst, mut staging) = (None, None, None);
            device
                .CreateTexture2D(&desc(FP16, D3D11_BIND_SHADER_RESOURCE.0 as u32, false), Some(&init), Some(&mut src))
                .expect("src");
            device
                .CreateTexture2D(
                    &desc(DXGI_FORMAT_B8G8R8A8_UNORM, D3D11_BIND_RENDER_TARGET.0 as u32, false),
                    None,
                    Some(&mut dst),
                )
                .expect("dst");
            device
                .CreateTexture2D(&desc(DXGI_FORMAT_B8G8R8A8_UNORM, 0, true), None, Some(&mut staging))
                .expect("staging");
            let (src, dst, staging) = (src.unwrap(), dst.unwrap(), staging.unwrap());
            let mut conv = ScrgbToSrgb::new(&device).expect("converter");
            conv.convert(&context, &src, &dst, (w, h), white).expect("convert");
            context.CopyResource(&staging, &dst);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).expect("map");
            let out = std::slice::from_raw_parts(mapped.pData.cast::<u8>(), (w * 4) as usize).to_vec();
            context.Unmap(&staging, 0);
            for (i, p) in pixels.iter().enumerate() {
                let want = [scrgb_to_srgb8(p[2], white), scrgb_to_srgb8(p[1], white), scrgb_to_srgb8(p[0], white)];
                let got = [out[i * 4], out[i * 4 + 1], out[i * 4 + 2]];
                for c in 0..3 {
                    assert!(want[c].abs_diff(got[c]) <= 1, "pixel {i}: BGR {got:?}, reference {want:?}");
                }
                assert_eq!(out[i * 4 + 3], 255, "opaque");
            }
        }
    }

    /// f32 to IEEE half (round to nearest even), enough for the test values (no subnormal halves needed).
    mod half_free {
        pub fn to_f16(v: f32) -> u16 {
            let bits = v.to_bits();
            let sign = ((bits >> 16) & 0x8000) as u16;
            let exp = ((bits >> 23) & 0xff) as i32 - 127 + 15;
            let mant = bits & 0x7f_ffff;
            if v == 0.0 {
                return sign;
            }
            if exp <= 0 {
                // Subnormal half.
                let m = (mant | 0x80_0000) >> (1 - exp + 13);
                return sign | m as u16;
            }
            let mut half = (exp as u32) << 10 | (mant >> 13);
            let rest = mant & 0x1fff;
            if rest > 0x1000 || (rest == 0x1000 && half & 1 == 1) {
                half += 1;
            }
            sign | half as u16
        }
    }
}
