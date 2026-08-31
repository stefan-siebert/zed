//! Runtime support for user-provided custom WGSL shaders on the Metal backend.
//!
//! Mirrors `gpui_wgpu::wgpu_renderer` and `gpui_windows::directx_custom_shader`:
//!
//! 1. The caller provides a WGSL fragment that defines `custom_effect(...)`.
//! 2. We wrap it in a full WGSL module whose `GlobalParams` matches what this
//!    renderer binds (just the viewport size — Metal has no shared globals
//!    cbuffer like DirectX does).
//! 3. The module is translated to MSL via `naga` and compiled at runtime with
//!    `MTLDevice::newLibraryWithSource:`.
//! 4. The resulting pipeline states are cached per `CustomShaderId`.
//!
//! The per-instance data is the scene's own `custom_shaders` array, written
//! into the frame's instance buffer like every other primitive and bound at
//! `CustomShaderInputIndex::Instances`. `[[instance_id]]` on Metal already
//! includes the base instance, so a batch binds the whole array and passes its
//! sub-range as the base instance — the same arrangement `draw_quads` uses.

use anyhow::{Context as _, Result};
use collections::HashMap;
use gpui::{CustomShaderId, CustomShaderInstance};

/// Buffer slots the custom-shader pipeline expects. `Vertices` is deliberately
/// left unbound — the vertex shader derives its unit quad from `[[vertex_id]]`
/// — but the index is reserved so the numbering matches every other pipeline in
/// `metal_renderer.rs`.
#[repr(C)]
pub(crate) enum CustomShaderInputIndex {
    #[expect(
        dead_code,
        reason = "reserved so slot numbering matches the other pipelines"
    )]
    Vertices = 0,
    Instances = 1,
    ViewportSize = 2,
    /// naga's `_mslBufferSizes`: the byte length of every runtime-sized array
    /// the module declares. There is exactly one here (`b_instances`), so the
    /// struct is a single `uint`.
    ///
    /// It has to be bound even though the generated code currently never reads
    /// it: naga adds the argument to any entry point that touches a
    /// runtime-sized array, whatever the bounds-check policy says, and a
    /// `custom_effect` calling `arrayLength()` would read it for real.
    InstancesByteLength = 3,
}

/// Uniform block bound at [`CustomShaderInputIndex::ViewportSize`].
///
/// Floats, not the `Size<DevicePixels>` (two `i32`) the built-in pipelines
/// bind: the WGSL template is shared in spirit with the wgpu backend, whose
/// `GlobalParams.viewport_size` is a `vec2<f32>`, and a custom shader that
/// wants pixel coordinates gets them from `position` anyway.
#[derive(Clone, Copy)]
#[repr(C)]
pub(crate) struct CustomShaderGlobals {
    pub(crate) viewport_size: [f32; 2],
}

/// Cache of compiled custom-shader pipelines.
pub(crate) struct CustomShaderResources {
    pipelines: HashMap<CustomShaderId, metal::RenderPipelineState>,
    next_id: u32,
}

impl CustomShaderResources {
    pub(crate) fn new() -> Self {
        Self {
            pipelines: HashMap::default(),
            next_id: 0,
        }
    }

    pub(crate) fn pipeline(&self, id: CustomShaderId) -> Option<&metal::RenderPipelineState> {
        self.pipelines.get(&id)
    }

    /// Compile the caller's WGSL fragment and cache the pipeline it produces.
    ///
    /// The fragment must define
    /// `fn custom_effect(position: vec2<f32>, bounds: Bounds, content_mask: Bounds, params: array<f32, 16>) -> vec4<f32>`.
    pub(crate) fn register(
        &mut self,
        device: &metal::Device,
        pixel_format: metal::MTLPixelFormat,
        wgsl_fragment: &str,
        label: &str,
    ) -> Result<CustomShaderId> {
        let wgsl = build_custom_shader_wgsl(wgsl_fragment);
        let translated = wgsl_to_msl(&wgsl)
            .with_context(|| format!("translating custom shader '{label}' to MSL"))?;

        let library = device
            .new_library_with_source(&translated.source, &metal::CompileOptions::new())
            .map_err(|message| {
                anyhow::anyhow!(
                    "compiling MSL for custom shader '{label}': {message}\n--- MSL ---\n{}",
                    translated.source
                )
            })?;

        let pipeline = build_custom_shader_pipeline_state(
            device,
            &library,
            label,
            &translated.vertex_entry_point,
            &translated.fragment_entry_point,
            pixel_format,
        )?;

        let id = CustomShaderId(self.next_id);
        self.next_id += 1;
        self.pipelines.insert(id, pipeline);
        Ok(id)
    }
}

/// Wrap the caller's fragment in a complete WGSL module.
///
/// Identical in structure to the wgpu and DirectX templates — the vertex stage,
/// the instance layout, and the clip-distance test all have to agree across the
/// three backends, since the same `custom_effect` source is expected to run on
/// all of them.
fn build_custom_shader_wgsl(user_fragment: &str) -> String {
    format!(
        r#"
struct GlobalParams {{
    viewport_size: vec2<f32>,
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
    pad: vec2<f32>,
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
"#
    )
}

/// MSL source plus the names naga gave the two entry points.
struct TranslatedShader {
    source: String,
    vertex_entry_point: String,
    fragment_entry_point: String,
}

/// Translate the wrapped WGSL module to MSL via naga.
///
/// Binding map:
///   * group=0, binding=0 (`globals` uniform)  → buffer(2), matching the
///     `ViewportSize` slot every other pipeline here uses
///   * group=1, binding=0 (`b_instances` SSBO) → buffer(1), the per-primitive
///     instance slot
///
/// Both entry points get both bindings: naga only emits the ones an entry point
/// actually reaches, so listing the unused pairing costs nothing and keeps the
/// map from having to track which stage reads what.
fn wgsl_to_msl(wgsl_source: &str) -> Result<TranslatedShader> {
    use naga::back::msl;

    let module = naga::front::wgsl::parse_str(wgsl_source).map_err(|error| {
        anyhow::anyhow!("WGSL parse error:\n{}", error.emit_to_string(wgsl_source))
    })?;
    let mut validator = naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::default(),
    );
    let module_info = validator
        .validate(&module)
        .map_err(|error| anyhow::anyhow!("WGSL validation error: {error}"))?;

    let mut resources = msl::BindingMap::default();
    resources.insert(
        naga::ResourceBinding {
            group: 0,
            binding: 0,
        },
        msl::BindTarget {
            buffer: Some(CustomShaderInputIndex::ViewportSize as u8),
            ..Default::default()
        },
    );
    resources.insert(
        naga::ResourceBinding {
            group: 1,
            binding: 0,
        },
        msl::BindTarget {
            buffer: Some(CustomShaderInputIndex::Instances as u8),
            ..Default::default()
        },
    );

    let mut options = msl::Options {
        // 2.0 is available on every macOS release gpui supports and is past
        // the 1.2 cutoff naga applies to storage buffers in fragment stages.
        lang_version: (2, 0),
        // Producing MSL that silently misses a binding would fail at draw time
        // with a blank quad rather than here with a message.
        fake_missing_bindings: false,
        bounds_check_policies: naga::proc::BoundsCheckPolicies {
            // The instance array is the only buffer, and the only index into it
            // is `[[instance_id]]`, which the draw call confines to the batch's
            // range. The default (`Restrict`) makes naga demand a "sizes
            // buffer" holding the runtime array length — a second buffer to
            // bind and keep in sync for a bound that already holds by
            // construction, as it does for every built-in pipeline in
            // `metal_renderer.rs`.
            buffer: naga::proc::BoundsCheckPolicy::Unchecked,
            ..Default::default()
        },
        ..Default::default()
    };
    for entry_point in &module.entry_points {
        options.per_entry_point_map.insert(
            entry_point.name.clone(),
            msl::EntryPointResources {
                resources: resources.clone(),
                sizes_buffer: Some(CustomShaderInputIndex::InstancesByteLength as u8),
                ..Default::default()
            },
        );
    }

    let pipeline_options = msl::PipelineOptions {
        // `None` writes every entry point, which is what we want: one library
        // holding both stages.
        entry_point: None,
        allow_and_force_point_size: false,
        // There are no vertex buffers — the quad comes from `[[vertex_id]]` —
        // so the pulling transform has nothing to rewrite and only adds
        // arguments this pipeline would then have to bind.
        vertex_pulling_transform: false,
        vertex_buffer_mappings: Vec::new(),
    };

    let (source, translation_info) =
        msl::write_string(&module, &module_info, &options, &pipeline_options)
            .map_err(|error| anyhow::anyhow!("MSL backend error: {error}"))?;

    // naga may rename entry points (MSL keywords, collisions); the translated
    // names come back in `module.entry_points` order.
    let mut vertex_entry_point = None;
    let mut fragment_entry_point = None;
    for (entry_point, name) in module
        .entry_points
        .iter()
        .zip(&translation_info.entry_point_names)
    {
        let name = name
            .as_ref()
            .map_err(|error| anyhow::anyhow!("entry point '{}': {error}", entry_point.name))?;
        match entry_point.stage {
            naga::ShaderStage::Vertex => vertex_entry_point = Some(name.clone()),
            naga::ShaderStage::Fragment => fragment_entry_point = Some(name.clone()),
            _ => {}
        }
    }

    Ok(TranslatedShader {
        source,
        vertex_entry_point: vertex_entry_point.context("no vertex entry point in custom shader")?,
        fragment_entry_point: fragment_entry_point
            .context("no fragment entry point in custom shader")?,
    })
}

/// Premultiplied-alpha blending, matching `wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING`
/// and the DirectX backend's blend state — a `custom_effect` returning a
/// half-transparent colour must composite the same way on all three.
///
/// This is *not* what `build_pipeline_state` in `metal_renderer.rs` sets up:
/// gpui's own primitives hand the blender straight (unpremultiplied) alpha.
fn build_custom_shader_pipeline_state(
    device: &metal::Device,
    library: &metal::LibraryRef,
    label: &str,
    vertex_fn_name: &str,
    fragment_fn_name: &str,
    pixel_format: metal::MTLPixelFormat,
) -> Result<metal::RenderPipelineState> {
    let vertex_fn = library
        .get_function(vertex_fn_name, None)
        .map_err(|message| anyhow::anyhow!("locating vertex function: {message}"))?;
    let fragment_fn = library
        .get_function(fragment_fn_name, None)
        .map_err(|message| anyhow::anyhow!("locating fragment function: {message}"))?;

    let descriptor = metal::RenderPipelineDescriptor::new();
    descriptor.set_label(label);
    descriptor.set_vertex_function(Some(vertex_fn.as_ref()));
    descriptor.set_fragment_function(Some(fragment_fn.as_ref()));
    let color_attachment = descriptor
        .color_attachments()
        .object_at(0)
        .context("render pipeline descriptor has no color attachment 0")?;
    color_attachment.set_pixel_format(pixel_format);
    color_attachment.set_blending_enabled(true);
    color_attachment.set_rgb_blend_operation(metal::MTLBlendOperation::Add);
    color_attachment.set_alpha_blend_operation(metal::MTLBlendOperation::Add);
    color_attachment.set_source_rgb_blend_factor(metal::MTLBlendFactor::One);
    color_attachment.set_source_alpha_blend_factor(metal::MTLBlendFactor::One);
    color_attachment.set_destination_rgb_blend_factor(metal::MTLBlendFactor::OneMinusSourceAlpha);
    color_attachment.set_destination_alpha_blend_factor(metal::MTLBlendFactor::OneMinusSourceAlpha);

    device
        .new_render_pipeline_state(&descriptor)
        .map_err(|message| anyhow::anyhow!("creating custom shader pipeline state: {message}"))
}

/// The instance layout the WGSL template declares has to stay byte-identical to
/// the Rust struct the renderer uploads; nothing else checks it.
const _: () = assert!(std::mem::size_of::<CustomShaderInstance>() == 112);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metal_renderer::MetalRenderer;
    use gpui::{Bounds, ContentMask, DevicePixels, ScaledPixels, Scene, Size, point, size};
    use parking_lot::Mutex;
    use std::sync::Arc;

    /// Paints the params as a flat colour, and reads `position`/`bounds` so a
    /// regression in either binding shows up as the wrong colour rather than as
    /// a silently unused argument.
    pub(super) const TEST_SHADER: &str = r#"
fn custom_effect(position: vec2<f32>, bounds: Bounds, content_mask: Bounds, params: array<f32, 16>) -> vec4<f32> {
    let u = (position.x - bounds.origin.x) / bounds.size.x;
    return vec4<f32>(params[0], params[1] * u, params[2], 1.0);
}
"#;

    fn scaled(left: f32, top: f32, width: f32, height: f32) -> Bounds<ScaledPixels> {
        Bounds::new(
            point(ScaledPixels(left), ScaledPixels(top)),
            size(ScaledPixels(width), ScaledPixels(height)),
        )
    }

    fn render(scene: &Scene, renderer: &mut MetalRenderer) -> image::RgbaImage {
        let viewport: Size<DevicePixels> = size(DevicePixels(64), DevicePixels(64));
        renderer
            .render_scene_to_image(scene, viewport)
            .expect("headless render failed")
    }

    #[test]
    fn custom_shader_paints_its_quad_and_honours_the_content_mask() {
        let mut renderer = MetalRenderer::new_headless(Arc::new(Mutex::new(
            crate::metal_renderer::InstanceBufferPool::default(),
        )));
        let shader_id = renderer
            .register_custom_shader(TEST_SHADER, "test-custom-shader")
            .expect("registering the test shader failed");

        // The quad spans x 16..48; the mask cuts its right half away at x = 32.
        let mut scene = Scene::default();
        scene.insert_primitive(CustomShaderInstance {
            order: 0,
            shader_id,
            bounds: scaled(16., 16., 32., 32.),
            content_mask: ContentMask {
                bounds: scaled(16., 16., 16., 32.),
            },
            params: {
                let mut params = [0.0; 16];
                params[0] = 1.0; // red: constant
                params[1] = 1.0; // green: ramps with the in-quad u coordinate
                params[2] = 0.0;
                params
            },
            _pad: [0.0; 2],
        });

        let image = render(&scene, &mut renderer);
        let pixel = |x: u32, y: u32| image.get_pixel(x, y).0;

        // Outside the quad: the clear colour.
        assert_eq!(pixel(4, 4), [0, 0, 0, 255], "outside the quad");
        // Inside the quad and the mask, a quarter of the way across (u = 0.25):
        // red full on, green ramping. Wide tolerance — this asserts that the
        // instance data arrived, not that the rasteriser is exact.
        let inside = pixel(24, 32);
        assert_eq!(inside[0], 255, "red channel inside the quad");
        assert!(
            (40..=90).contains(&inside[1]),
            "green should ramp with u; got {inside:?}"
        );
        assert_eq!(inside[2], 0, "blue channel inside the quad");
        // Inside the quad but outside the content mask: nothing painted.
        assert_eq!(pixel(40, 32), [0, 0, 0, 255], "clipped by the content mask");
    }

    /// A second batch starts at a non-zero instance index. Metal's
    /// `[[instance_id]]` counts from the base instance, so the batch binds the
    /// whole array and passes `range.start` as the base — get that wrong and
    /// the second quad paints the *first* instance's params and bounds.
    ///
    /// Two registrations of the same source, because batches are split by
    /// shader id: one id would put both instances in a single batch based at 0,
    /// which is exactly the case that cannot catch the bug.
    #[test]
    fn a_batch_starting_past_instance_zero_reads_its_own_instance() {
        let mut renderer = MetalRenderer::new_headless(Arc::new(Mutex::new(
            crate::metal_renderer::InstanceBufferPool::default(),
        )));
        let first = renderer
            .register_custom_shader(TEST_SHADER, "first")
            .expect("registering the first shader failed");
        let second = renderer
            .register_custom_shader(TEST_SHADER, "second")
            .expect("registering the second shader failed");

        let flat = |red: f32, blue: f32| {
            let mut params = [0.0; 16];
            params[0] = red;
            params[2] = blue;
            params
        };
        let mut scene = Scene::default();
        // Left quad, red, instance 0.
        scene.insert_primitive(CustomShaderInstance {
            order: 0,
            shader_id: first,
            bounds: scaled(0., 0., 32., 64.),
            content_mask: ContentMask {
                bounds: scaled(0., 0., 64., 64.),
            },
            params: flat(1.0, 0.0),
            _pad: [0.0; 2],
        });
        // Right quad, blue, instance 1 — different bounds AND different params,
        // so either half of a base-instance mix-up is visible.
        scene.insert_primitive(CustomShaderInstance {
            order: 0,
            shader_id: second,
            bounds: scaled(32., 0., 32., 64.),
            content_mask: ContentMask {
                bounds: scaled(0., 0., 64., 64.),
            },
            params: flat(0.0, 1.0),
            _pad: [0.0; 2],
        });

        let image = render(&scene, &mut renderer);
        assert_eq!(image.get_pixel(8, 32).0[0], 255, "left quad should be red");
        assert_eq!(image.get_pixel(8, 32).0[2], 0, "left quad should be red");
        assert_eq!(
            image.get_pixel(56, 32).0[2],
            255,
            "right quad should be blue"
        );
        assert_eq!(image.get_pixel(56, 32).0[0], 0, "right quad should be blue");
    }

    #[test]
    fn invalid_wgsl_is_reported_rather_than_compiled() {
        let mut renderer = MetalRenderer::new_headless(Arc::new(Mutex::new(
            crate::metal_renderer::InstanceBufferPool::default(),
        )));
        assert!(
            renderer
                .register_custom_shader("fn custom_effect() -> {", "broken")
                .is_none(),
            "malformed WGSL must not yield a shader id"
        );
    }
}
