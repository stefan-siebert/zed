//! Runtime support for user-provided custom WGSL shaders on the DirectX backend.
//!
//! Mirrors the wgpu implementation in `gpui_wgpu::wgpu_renderer`:
//!
//! 1. The user provides a WGSL fragment that defines `custom_effect(...)`.
//! 2. We wrap it in a full WGSL module that matches the existing DirectX
//!    `GlobalParams` cbuffer layout (gamma_ratios, viewport_size, ...).
//! 3. The module is translated to HLSL via `naga` and compiled to D3D11
//!    bytecode at runtime via `D3DCompile` (Shader Model 5.0).
//! 4. The compiled vertex/pixel shaders and the per-instance data buffer
//!    (a raw `ByteAddressBuffer` at register `t1`, matching the slot used by
//!    the other render pipelines) live in `CustomShaderResources`.

use std::{collections::HashMap, slice};

use anyhow::{Context, Result};
use windows::{
    Win32::Graphics::{
        Direct3D::{Fxc::D3DCompile, ID3DBlob, D3D11_SRV_DIMENSION_BUFFEREX},
        Direct3D11::*,
        Dxgi::Common::DXGI_FORMAT_R32_TYPELESS,
    },
    core::PCSTR,
};
#[cfg(debug_assertions)]
use windows::Win32::Graphics::Direct3D::Fxc::D3DCOMPILE_SKIP_OPTIMIZATION;
#[cfg(not(debug_assertions))]
use windows::Win32::Graphics::Direct3D::Fxc::D3DCOMPILE_OPTIMIZATION_LEVEL3;

use gpui::{CustomShaderId, CustomShaderInstance};

/// Size in bytes of one `CustomShaderInstance` (must match the WGSL storage
/// layout exactly — see the `build_custom_shader_wgsl` template).
const CUSTOM_SHADER_INSTANCE_SIZE: usize = std::mem::size_of::<CustomShaderInstance>();

/// One compiled custom shader pipeline.
pub(crate) struct CustomShaderPipeline {
    pub(crate) vertex: ID3D11VertexShader,
    pub(crate) fragment: ID3D11PixelShader,
    pub(crate) blend_state: ID3D11BlendState,
}

/// Owns the cache of compiled custom shaders and the shared raw instance buffer.
pub(crate) struct CustomShaderResources {
    pipelines: HashMap<CustomShaderId, CustomShaderPipeline>,
    next_id: u32,
    instance_buffer: Option<ID3D11Buffer>,
    instance_buffer_capacity: usize,
}

impl CustomShaderResources {
    pub(crate) fn new() -> Self {
        Self {
            pipelines: HashMap::new(),
            next_id: 0,
            instance_buffer: None,
            instance_buffer_capacity: 0,
        }
    }

    pub(crate) fn pipeline(&self, id: CustomShaderId) -> Option<&CustomShaderPipeline> {
        self.pipelines.get(&id)
    }

    pub(crate) fn instance_buffer(&self) -> Option<&ID3D11Buffer> {
        self.instance_buffer.as_ref()
    }

    /// Compile a new custom shader from the user's WGSL fragment and cache the pipeline.
    /// The fragment must define
    /// `fn custom_effect(pos: vec2<f32>, bounds: Bounds, content_mask: Bounds, params: array<f32, 16>) -> vec4<f32>`.
    pub(crate) fn register(
        &mut self,
        device: &ID3D11Device,
        wgsl_fragment: &str,
        label: &str,
    ) -> Result<CustomShaderId> {
        let id = CustomShaderId(self.next_id);

        let full_wgsl = build_custom_shader_wgsl(wgsl_fragment);
        let hlsl = wgsl_to_hlsl(&full_wgsl)
            .with_context(|| format!("translating custom shader '{label}' to HLSL"))?;

        let vs_blob = compile_hlsl(&hlsl, "vs_custom", "vs_5_0", label)
            .with_context(|| format!("compiling vertex shader for custom shader '{label}'"))?;
        let ps_blob = compile_hlsl(&hlsl, "fs_custom", "ps_5_0", label)
            .with_context(|| format!("compiling pixel shader for custom shader '{label}'"))?;

        let vertex = create_vertex_shader(device, blob_bytes(&vs_blob))?;
        let fragment = create_pixel_shader(device, blob_bytes(&ps_blob))?;
        let blend_state = create_premultiplied_blend_state(device)?;

        self.pipelines.insert(
            id,
            CustomShaderPipeline {
                vertex,
                fragment,
                blend_state,
            },
        );
        self.next_id += 1;
        Ok(id)
    }

    /// Upload the entire `custom_shaders` slice for the current frame to the GPU,
    /// growing the buffer to a power-of-two capacity if needed.
    pub(crate) fn upload_instances(
        &mut self,
        device: &ID3D11Device,
        device_context: &ID3D11DeviceContext,
        instances: &[CustomShaderInstance],
    ) -> Result<()> {
        if instances.is_empty() {
            return Ok(());
        }
        if self.instance_buffer.is_none() || self.instance_buffer_capacity < instances.len() {
            let new_capacity = instances.len().next_power_of_two().max(4);
            self.instance_buffer = Some(create_raw_instance_buffer(device, new_capacity)?);
            self.instance_buffer_capacity = new_capacity;
        }
        let buffer = self
            .instance_buffer
            .as_ref()
            .context("instance buffer missing after grow")?;
        unsafe {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            device_context
                .Map(buffer, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))
                .context("mapping custom shader instance buffer")?;
            std::ptr::copy_nonoverlapping(
                instances.as_ptr() as *const u8,
                mapped.pData as *mut u8,
                instances.len() * CUSTOM_SHADER_INSTANCE_SIZE,
            );
            device_context.Unmap(buffer, 0);
        }
        Ok(())
    }

    /// Recreate this resource set after device-lost recovery.
    /// Pipeline cache is cleared since the compiled shaders belong to the old device;
    /// the application must re-register its shaders.
    pub(crate) fn handle_device_lost(&mut self) {
        self.pipelines.clear();
        self.instance_buffer = None;
        self.instance_buffer_capacity = 0;
        // Note: we keep `next_id` so previously-issued IDs become permanently invalid
        // rather than colliding with newly-registered shaders.
    }
}

/// Create the raw (`D3D11_RESOURCE_MISC_BUFFER_ALLOW_RAW_VIEWS`) buffer that backs the
/// `ByteAddressBuffer` naga emits for the storage binding.
fn create_raw_instance_buffer(
    device: &ID3D11Device,
    instance_capacity: usize,
) -> Result<ID3D11Buffer> {
    let byte_width =
        (instance_capacity * CUSTOM_SHADER_INSTANCE_SIZE).max(CUSTOM_SHADER_INSTANCE_SIZE);
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: byte_width as u32,
        Usage: D3D11_USAGE_DYNAMIC,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
        MiscFlags: D3D11_RESOURCE_MISC_BUFFER_ALLOW_RAW_VIEWS.0 as u32,
        StructureByteStride: 0,
    };
    let mut buffer = None;
    unsafe { device.CreateBuffer(&desc, None, Some(&mut buffer))? };
    buffer.context("CreateBuffer for custom shader instances returned None")
}

/// Create a raw (`BUFFEREX`) SRV that exposes only `[first_instance .. first_instance + instance_count)`.
/// `ByteAddressBuffer` indices are in 4-byte words, so `FirstElement` / `NumElements`
/// must be converted from instance counts.
pub(crate) fn create_raw_instance_buffer_srv(
    device: &ID3D11Device,
    buffer: &ID3D11Buffer,
    first_instance: usize,
    instance_count: usize,
) -> Result<Option<ID3D11ShaderResourceView>> {
    debug_assert!(CUSTOM_SHADER_INSTANCE_SIZE.is_multiple_of(4));
    let words_per_instance = CUSTOM_SHADER_INSTANCE_SIZE / 4;
    let first_word = (first_instance * words_per_instance) as u32;
    let num_words = (instance_count * words_per_instance) as u32;
    let desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
        Format: DXGI_FORMAT_R32_TYPELESS,
        ViewDimension: D3D11_SRV_DIMENSION_BUFFEREX,
        Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
            BufferEx: D3D11_BUFFEREX_SRV {
                FirstElement: first_word,
                NumElements: num_words,
                Flags: D3D11_BUFFEREX_SRV_FLAG_RAW.0 as u32,
            },
        },
    };
    let mut view = None;
    unsafe { device.CreateShaderResourceView(buffer, Some(&desc), Some(&mut view))? };
    Ok(view)
}

/// Build a complete WGSL module from the user's fragment.
///
/// The `GlobalParams` struct is laid out to match the existing DirectX renderer's
/// global params cbuffer (`gamma_ratios`, `viewport_size`, `grayscale_enhanced_contrast`,
/// `subpixel_enhanced_contrast`) so that the same cbuffer at register `b0` can be reused.
fn build_custom_shader_wgsl(user_fragment: &str) -> String {
    format!(
        r#"
struct GlobalParams {{
    gamma_ratios: vec4<f32>,
    viewport_size: vec2<f32>,
    grayscale_enhanced_contrast: f32,
    subpixel_enhanced_contrast: f32,
}}
@group(0) @binding(0) var<uniform> globals: GlobalParams;

struct Bounds {{
    origin: vec2<f32>,
    size: vec2<f32>,
}}

struct CustomShaderInstance {{
    order: u32,
    shader_id: u32,
    bounds: Bounds,
    content_mask: Bounds,
    params: array<f32, 16>,
}}
@group(1) @binding(0) var<storage, read> b_instances: array<CustomShaderInstance>;

fn to_device_position(unit_vertex: vec2<f32>, bounds: Bounds) -> vec4<f32> {{
    let pos = unit_vertex * bounds.size + bounds.origin;
    let ndc = pos / globals.viewport_size * vec2<f32>(2.0, -2.0) + vec2<f32>(-1.0, 1.0);
    return vec4<f32>(ndc, 0.0, 1.0);
}}

fn distance_from_clip_rect(unit_vertex: vec2<f32>, bounds: Bounds, clip: Bounds) -> vec4<f32> {{
    let pos = unit_vertex * bounds.size + bounds.origin;
    return vec4<f32>(
        pos.x - clip.origin.x,
        clip.origin.x + clip.size.x - pos.x,
        pos.y - clip.origin.y,
        clip.origin.y + clip.size.y - pos.y,
    );
}}

struct CustomVarying {{
    @builtin(position) position: vec4<f32>,
    @location(0) @interpolate(flat) instance_id: u32,
    @location(1) clip_distances: vec4<f32>,
}}

@vertex
fn vs_custom(@builtin(vertex_index) vertex_id: u32, @builtin(instance_index) instance_id: u32) -> CustomVarying {{
    let unit_vertex = vec2<f32>(f32(vertex_id & 1u), 0.5 * f32(vertex_id & 2u));
    let inst = b_instances[instance_id];
    var out: CustomVarying;
    out.position = to_device_position(unit_vertex, inst.bounds);
    out.instance_id = instance_id;
    out.clip_distances = distance_from_clip_rect(unit_vertex, inst.bounds, inst.content_mask);
    return out;
}}

// --- User-provided fragment shader ---
{user_fragment}

@fragment
fn fs_custom(input: CustomVarying) -> @location(0) vec4<f32> {{
    if (any(input.clip_distances < vec4<f32>(0.0))) {{
        return vec4<f32>(0.0);
    }}
    let inst = b_instances[input.instance_id];
    return custom_effect(input.position.xy, inst.bounds, inst.content_mask, inst.params);
}}
"#,
        user_fragment = user_fragment,
    )
}

/// Translate WGSL to HLSL via naga.
///
/// Binding map matches the existing DirectX renderer:
///   * group=0, binding=0 (`globals` uniform)  → `b0`  (shared GlobalParams cbuffer)
///   * group=1, binding=0 (`b_instances` SSBO) → `t1`  (shared instance buffer slot)
fn wgsl_to_hlsl(wgsl_source: &str) -> Result<String> {
    use naga::back::hlsl;

    let module = naga::front::wgsl::parse_str(wgsl_source).map_err(|err| {
        anyhow::anyhow!("WGSL parse error:\n{}", err.emit_to_string(wgsl_source))
    })?;
    let mut validator = naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::default(),
    );
    let module_info = validator
        .validate(&module)
        .map_err(|err| anyhow::anyhow!("WGSL validation error: {err}"))?;

    let mut options = hlsl::Options::default();
    options.shader_model = hlsl::ShaderModel::V5_0;
    options.fake_missing_bindings = false;
    options.binding_map.insert(
        naga::ResourceBinding {
            group: 0,
            binding: 0,
        },
        hlsl::BindTarget {
            space: 0,
            register: 0,
            binding_array_size: None,
            dynamic_storage_buffer_offsets_index: None,
            restrict_indexing: false,
        },
    );
    options.binding_map.insert(
        naga::ResourceBinding {
            group: 1,
            binding: 0,
        },
        hlsl::BindTarget {
            space: 0,
            register: 1,
            binding_array_size: None,
            dynamic_storage_buffer_offsets_index: None,
            restrict_indexing: false,
        },
    );

    let pipeline_options = hlsl::PipelineOptions::default();
    let mut hlsl_source = String::new();
    let mut writer = hlsl::Writer::new(&mut hlsl_source, &options, &pipeline_options);
    writer
        .write(&module, &module_info, None)
        .map_err(|err| anyhow::anyhow!("HLSL backend error: {err}"))?;
    Ok(hlsl_source)
}

/// Compile an HLSL string to a shader blob via D3DCompile.
fn compile_hlsl(hlsl_source: &str, entry: &str, target: &str, label: &str) -> Result<ID3DBlob> {
    let entry_cstr = std::ffi::CString::new(entry).context("entry name contains NUL")?;
    let target_cstr = std::ffi::CString::new(target).context("target name contains NUL")?;
    let source_name_cstr =
        std::ffi::CString::new(label).context("custom shader label contains NUL")?;

    #[cfg(debug_assertions)]
    let flags = D3DCOMPILE_SKIP_OPTIMIZATION;
    #[cfg(not(debug_assertions))]
    let flags = D3DCOMPILE_OPTIMIZATION_LEVEL3;

    let mut blob = None;
    let mut error_blob = None;
    let result = unsafe {
        D3DCompile(
            hlsl_source.as_ptr() as *const _,
            hlsl_source.len(),
            PCSTR::from_raw(source_name_cstr.as_ptr() as *const u8),
            None,
            None,
            PCSTR::from_raw(entry_cstr.as_ptr() as *const u8),
            PCSTR::from_raw(target_cstr.as_ptr() as *const u8),
            flags,
            0,
            &mut blob,
            Some(&mut error_blob),
        )
    };
    if let Err(error) = result {
        let message = if let Some(error_blob) = error_blob.as_ref() {
            unsafe {
                std::ffi::CStr::from_ptr(error_blob.GetBufferPointer() as *const i8)
                    .to_string_lossy()
                    .into_owned()
            }
        } else {
            String::new()
        };
        anyhow::bail!(
            "D3DCompile failed for entry `{entry}` ({target}): {error:?}: {message}\n--- HLSL ---\n{hlsl_source}"
        );
    }
    blob.context("D3DCompile returned no blob")
}

fn blob_bytes(blob: &ID3DBlob) -> &[u8] {
    unsafe { slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize()) }
}

fn create_vertex_shader(device: &ID3D11Device, bytes: &[u8]) -> Result<ID3D11VertexShader> {
    unsafe {
        let mut shader = None;
        device.CreateVertexShader(bytes, None, Some(&mut shader))?;
        shader.context("CreateVertexShader returned None")
    }
}

fn create_pixel_shader(device: &ID3D11Device, bytes: &[u8]) -> Result<ID3D11PixelShader> {
    unsafe {
        let mut shader = None;
        device.CreatePixelShader(bytes, None, Some(&mut shader))?;
        shader.context("CreatePixelShader returned None")
    }
}

/// Premultiplied-alpha blending: matches `wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING`.
fn create_premultiplied_blend_state(device: &ID3D11Device) -> Result<ID3D11BlendState> {
    let mut desc = D3D11_BLEND_DESC::default();
    desc.RenderTarget[0].BlendEnable = true.into();
    desc.RenderTarget[0].BlendOp = D3D11_BLEND_OP_ADD;
    desc.RenderTarget[0].BlendOpAlpha = D3D11_BLEND_OP_ADD;
    desc.RenderTarget[0].SrcBlend = D3D11_BLEND_ONE;
    desc.RenderTarget[0].SrcBlendAlpha = D3D11_BLEND_ONE;
    desc.RenderTarget[0].DestBlend = D3D11_BLEND_INV_SRC_ALPHA;
    desc.RenderTarget[0].DestBlendAlpha = D3D11_BLEND_INV_SRC_ALPHA;
    desc.RenderTarget[0].RenderTargetWriteMask = D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8;
    let mut state = None;
    unsafe { device.CreateBlendState(&desc, Some(&mut state))? };
    state.context("CreateBlendState returned None")
}
