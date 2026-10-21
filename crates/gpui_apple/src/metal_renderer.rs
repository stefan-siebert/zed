use crate::metal_atlas::MetalAtlas;
use crate::metal_custom_shader::{
    CustomShaderGlobals, CustomShaderInputIndex, CustomShaderResources,
};
use anyhow::{Context as _, Result};
use block2::RcBlock;
use core_graphics::geometry::CGSize;
use gpui::{
    AtlasTextureId, BackdropBlur, Background, Bounds, ContentMask, CustomShaderId, DevicePixels,
    PaintSurface, Path, Point, PrimitiveBatch, ScaledPixels, Scene, Size, point, size,
};
use image::RgbaImage;
use objc2::runtime::AnyObject;

use core_graphics::color_space::{CGColorSpace, kCGColorSpaceDisplayP3};
use foreign_types::{ForeignType, ForeignTypeRef};
use metal::{
    CAMetalLayer, CommandQueue, MTLGPUFamily, MTLPixelFormat, MTLResourceOptions, NSRange,
    NSUInteger,
};
use objc2_core_foundation::CFRetained;
use objc2_core_video::{
    CVMetalTexture, CVMetalTextureCache, CVMetalTextureGetTexture, CVPixelBufferGetHeight,
    CVPixelBufferGetHeightOfPlane, CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth,
    CVPixelBufferGetWidthOfPlane, kCVPixelFormatType_420YpCbCr8BiPlanarFullRange, kCVReturnSuccess,
};
use parking_lot::Mutex;

use std::{cell::Cell, ffi::c_void, mem, mem::MaybeUninit, ops::Range, ptr, slice, sync::Arc};

// Exported to metal
pub(crate) type PointF = gpui::Point<f32>;

#[cfg(not(feature = "runtime_shaders"))]
const SHADERS_METALLIB: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/shaders.metallib"));
#[cfg(feature = "runtime_shaders")]
const SHADERS_SOURCE_FILE: &str = include_str!(concat!(env!("OUT_DIR"), "/stitched_shaders.metal"));
// Use 4x MSAA, all devices support it.
// https://developer.apple.com/documentation/metal/mtldevice/1433355-supportstexturesamplecount
const PATH_SAMPLE_COUNT: u32 = 4;
/// Metal requires the offset a buffer is bound at to be 256-byte aligned.
const INSTANCE_BUFFER_ALIGNMENT: usize = 256;
const MAX_INSTANCE_BUFFER_SIZE: usize = 256 * 1024 * 1024;

pub type Context = Arc<Mutex<InstanceBufferPool>>;
pub type Renderer = MetalRenderer;

pub unsafe fn new_renderer(
    context: self::Context,
    _native_window: *mut c_void,
    _native_view: *mut c_void,
    _bounds: gpui::Size<f32>,
    transparent: bool,
    use_display_p3: bool,
) -> Renderer {
    MetalRenderer::new(context, transparent, use_display_p3)
}

pub struct InstanceBufferPool {
    buffer_size: usize,
    buffers: Vec<metal::Buffer>,
}

impl Default for InstanceBufferPool {
    fn default() -> Self {
        Self {
            buffer_size: 2 * 1024 * 1024,
            buffers: Vec::new(),
        }
    }
}

pub(crate) struct InstanceBuffer {
    metal_buffer: metal::Buffer,
    size: usize,
}

impl InstanceBufferPool {
    pub(crate) fn reset(&mut self, buffer_size: usize) {
        self.buffer_size = buffer_size;
        self.buffers.clear();
    }

    pub(crate) fn acquire(
        &mut self,
        device: &metal::Device,
        unified_memory: bool,
    ) -> InstanceBuffer {
        let buffer = self.buffers.pop().unwrap_or_else(|| {
            let options = if unified_memory {
                MTLResourceOptions::StorageModeShared
                    // Buffers are write only which can benefit from the combined cache
                    // https://developer.apple.com/documentation/metal/mtlresourceoptions/cpucachemodewritecombined
                    | MTLResourceOptions::CPUCacheModeWriteCombined
            } else {
                MTLResourceOptions::StorageModeManaged
            };

            device.new_buffer(self.buffer_size as u64, options)
        });
        InstanceBuffer {
            metal_buffer: buffer,
            size: self.buffer_size,
        }
    }

    pub(crate) fn release(&mut self, buffer: InstanceBuffer) {
        if buffer.size == self.buffer_size {
            self.buffers.push(buffer.metal_buffer)
        }
    }
}

pub struct MetalRenderer {
    device: metal::Device,
    layer: Option<metal::MetalLayer>,
    is_apple_gpu: bool,
    is_unified_memory: bool,
    presents_with_transaction: bool,
    /// For headless rendering, tracks whether output should be opaque
    opaque: bool,
    command_queue: CommandQueue,
    paths_rasterization_pipeline_state: metal::RenderPipelineState,
    path_sprites_pipeline_state: metal::RenderPipelineState,
    shadows_pipeline_state: metal::RenderPipelineState,
    quads_pipeline_state: metal::RenderPipelineState,
    underlines_pipeline_state: metal::RenderPipelineState,
    monochrome_sprites_pipeline_state: metal::RenderPipelineState,
    polychrome_sprites_pipeline_state: metal::RenderPipelineState,
    surfaces_pipeline_state: metal::RenderPipelineState,
    backdrop_blur_pipeline_state: metal::RenderPipelineState,
    kawase_down_pipeline_state: metal::RenderPipelineState,
    kawase_up_pipeline_state: metal::RenderPipelineState,
    unit_vertices: metal::Buffer,
    #[allow(clippy::arc_with_non_send_sync)]
    instance_buffer_pool: Arc<Mutex<InstanceBufferPool>>,
    sprite_atlas: Arc<MetalAtlas>,
    core_video_texture_cache: CFRetained<CVMetalTextureCache>,
    path_intermediate_texture: Option<metal::Texture>,
    path_intermediate_msaa_texture: Option<metal::Texture>,
    /// Half-resolution ping-pong pair for the backdrop-blur chain. Half res
    /// because a Kawase chain is already a low-pass filter — the detail the
    /// full-resolution copy would carry is exactly what gets thrown away.
    backdrop_blur_textures: Option<[metal::Texture; 2]>,
    /// Pipelines compiled from caller-provided WGSL, see `metal_custom_shader`.
    /// Empty until someone calls `register_custom_shader`.
    custom_shaders: CustomShaderResources,
    path_sample_count: u32,
    /// Offscreen render target reused across `render_scene` calls when
    /// rendering headlessly without reading pixels back.
    #[cfg(any(test, feature = "bench-support", feature = "test-support"))]
    headless_render_target: Option<metal::Texture>,
}

#[repr(C)]
pub struct PathRasterizationVertex {
    pub xy_position: Point<ScaledPixels>,
    pub st_position: Point<f32>,
    pub color: Background,
    pub bounds: Bounds<ScaledPixels>,
}

impl MetalRenderer {
    /// Creates a new MetalRenderer with a CAMetalLayer for window-based rendering.
    pub fn new(
        instance_buffer_pool: Arc<Mutex<InstanceBufferPool>>,
        transparent: bool,
        use_display_p3: bool,
    ) -> Self {
        let device = Self::create_device();
        let layer = metal::MetalLayer::new();
        Self::configure_layer(&layer, &device, transparent, use_display_p3);
        Self::new_internal(device, Some(layer), !transparent, instance_buffer_pool)
    }

    /// Creates a renderer for a CAMetalLayer owned by a platform view, such as
    /// the backing layer UIKit creates for a view whose `layerClass` is
    /// `CAMetalLayer`. The renderer retains the layer.
    pub fn from_layer(
        instance_buffer_pool: Arc<Mutex<InstanceBufferPool>>,
        layer: &objc2_quartz_core::CAMetalLayer,
        transparent: bool,
    ) -> Self {
        let device = Self::create_device();
        // Both types bind the same Objective-C class, so this only changes which
        // Rust wrapper views the live layer. `to_owned` retains it.
        let layer = unsafe {
            metal::MetalLayerRef::from_ptr(ptr::from_ref(layer).cast_mut().cast::<CAMetalLayer>())
        }
        .to_owned();
        Self::configure_layer(&layer, &device, transparent, false);
        Self::new_internal(device, Some(layer), !transparent, instance_buffer_pool)
    }

    fn configure_layer(
        layer: &metal::MetalLayerRef,
        device: &metal::DeviceRef,
        transparent: bool,
        use_display_p3: bool,
    ) {
        layer.set_device(device);
        layer.set_pixel_format(MTLPixelFormat::BGRA8Unorm);
        // metal-rs doesn't bind these setters, so view the same object through
        // objc2's typed CAMetalLayer binding.
        let objc2_layer: &objc2_quartz_core::CAMetalLayer = unsafe { &*layer.as_ptr().cast() };
        if use_display_p3 {
            // SAFETY: The constant is extern, from Core Graphics
            let display_p3_name = unsafe { kCGColorSpaceDisplayP3 };
            if let Some(display_p3_color_space) = CGColorSpace::create_with_name(display_p3_name) {
                // SAFETY: `layer` is a CAMetalLayer and the color space remains valid for the call.
                // 'metal-rs' does not expose CAMetalLayer's typed `colorspace` setter.
                // Could be replaced with a native binding via objc2-* in future
                unsafe {
                    let _: () = objc2::msg_send![
                        objc2_layer,
                        setColorspace: display_p3_color_space.as_ptr().cast::<c_void>()
                    ];
                }
            } else {
                log::error!("unable to create the Display P3 color space");
            }
        }
        // Support direct-to-display rendering if the window is not transparent
        // https://developer.apple.com/documentation/metal/managing-your-game-window-for-metal-in-macos
        layer.set_opaque(!transparent);
        layer.set_maximum_drawable_count(3);
        // The drawable must be readable, not just writable:
        //   - backdrop blur samples the frame painted so far (see
        //     `blur_backdrop`), and
        //   - visual tests capture screenshots without ScreenCaptureKit.
        // This forgoes some display-path optimisations, which is why Apple
        // defaults it the other way; both features need it.
        layer.set_framebuffer_only(false);
        objc2_layer.setAllowsNextDrawableTimeout(false);
        objc2_layer.setNeedsDisplayOnBoundsChange(true);
        // UIKit sizes a view's backing layer itself; only AppKit-hosted
        // layers need to track their superlayer's bounds.
        #[cfg(target_os = "macos")]
        objc2_layer.setAutoresizingMask(
            objc2_quartz_core::CAAutoresizingMask::LayerWidthSizable
                | objc2_quartz_core::CAAutoresizingMask::LayerHeightSizable,
        );
    }

    /// Creates a new headless MetalRenderer for offscreen rendering without a window.
    ///
    /// This renderer can render scenes to images without requiring a CAMetalLayer,
    /// window, or AppKit. Use `render_scene_to_image()` to render scenes.
    #[cfg(any(test, feature = "bench-support", feature = "test-support"))]
    pub fn new_headless(instance_buffer_pool: Arc<Mutex<InstanceBufferPool>>) -> Self {
        let device = Self::create_device();
        Self::new_internal(device, None, true, instance_buffer_pool)
    }

    #[cfg(target_os = "macos")]
    fn create_device() -> metal::Device {
        // Prefer low‐power integrated GPUs on Intel Mac. On Apple
        // Silicon, there is only ever one GPU, so this is equivalent to
        // `metal::Device::system_default()`.
        if let Some(d) = metal::Device::all()
            .into_iter()
            .min_by_key(|d| (d.is_removable(), !d.is_low_power()))
        {
            d
        } else {
            // For some reason `all()` can return an empty list, see https://github.com/zed-industries/zed/issues/37689
            // In that case, we fall back to the system default device.
            log::error!(
                "Unable to enumerate Metal devices; attempting to use system default device"
            );
            Self::system_default_device()
        }
    }

    #[cfg(target_os = "ios")]
    fn create_device() -> metal::Device {
        Self::system_default_device()
    }

    fn system_default_device() -> metal::Device {
        metal::Device::system_default().unwrap_or_else(|| {
            log::error!("unable to access a compatible graphics device");
            std::process::exit(1);
        })
    }

    fn new_internal(
        device: metal::Device,
        layer: Option<metal::MetalLayer>,
        opaque: bool,
        instance_buffer_pool: Arc<Mutex<InstanceBufferPool>>,
    ) -> Self {
        #[cfg(feature = "runtime_shaders")]
        let library = device
            .new_library_with_source(&SHADERS_SOURCE_FILE, &metal::CompileOptions::new())
            .expect("error building metal library");
        #[cfg(not(feature = "runtime_shaders"))]
        let library = device
            .new_library_with_data(SHADERS_METALLIB)
            .expect("error building metal library");

        fn to_float2_bits(point: PointF) -> u64 {
            let mut output = point.y.to_bits() as u64;
            output <<= 32;
            output |= point.x.to_bits() as u64;
            output
        }

        // Shared memory can be used only if CPU and GPU share the same memory space.
        // https://developer.apple.com/documentation/metal/setting-resource-storage-modes
        // iOS does not support managed resources. Its simulator may report a
        // non-unified host GPU even though resources must still use shared storage.
        let is_unified_memory = cfg!(target_os = "ios") || device.has_unified_memory();
        // Apple GPU families support memoryless textures, which can significantly reduce
        // memory usage by keeping render targets in on-chip tile memory instead of
        // allocating backing store in system memory.
        // https://developer.apple.com/documentation/metal/mtlgpufamily
        let is_apple_gpu = device.supports_family(MTLGPUFamily::Apple1);

        let unit_vertices = [
            to_float2_bits(point(0., 0.)),
            to_float2_bits(point(1., 0.)),
            to_float2_bits(point(0., 1.)),
            to_float2_bits(point(0., 1.)),
            to_float2_bits(point(1., 0.)),
            to_float2_bits(point(1., 1.)),
        ];
        let unit_vertices = device.new_buffer_with_data(
            unit_vertices.as_ptr() as *const c_void,
            mem::size_of_val(&unit_vertices) as u64,
            if is_unified_memory {
                MTLResourceOptions::StorageModeShared
                    | MTLResourceOptions::CPUCacheModeWriteCombined
            } else {
                MTLResourceOptions::StorageModeManaged
            },
        );

        let paths_rasterization_pipeline_state = build_path_rasterization_pipeline_state(
            &device,
            &library,
            "paths_rasterization",
            "path_rasterization_vertex",
            "path_rasterization_fragment",
            MTLPixelFormat::BGRA8Unorm,
            PATH_SAMPLE_COUNT,
        );
        let path_sprites_pipeline_state = build_path_sprite_pipeline_state(
            &device,
            &library,
            "path_sprites",
            "path_sprite_vertex",
            "path_sprite_fragment",
            MTLPixelFormat::BGRA8Unorm,
        );
        let shadows_pipeline_state = build_pipeline_state(
            &device,
            &library,
            "shadows",
            "shadow_vertex",
            "shadow_fragment",
            MTLPixelFormat::BGRA8Unorm,
        );
        let quads_pipeline_state = build_pipeline_state(
            &device,
            &library,
            "quads",
            "quad_vertex",
            "quad_fragment",
            MTLPixelFormat::BGRA8Unorm,
        );
        let underlines_pipeline_state = build_pipeline_state(
            &device,
            &library,
            "underlines",
            "underline_vertex",
            "underline_fragment",
            MTLPixelFormat::BGRA8Unorm,
        );
        let monochrome_sprites_pipeline_state = build_pipeline_state(
            &device,
            &library,
            "monochrome_sprites",
            "monochrome_sprite_vertex",
            "monochrome_sprite_fragment",
            MTLPixelFormat::BGRA8Unorm,
        );
        let polychrome_sprites_pipeline_state = build_pipeline_state(
            &device,
            &library,
            "polychrome_sprites",
            "polychrome_sprite_vertex",
            "polychrome_sprite_fragment",
            MTLPixelFormat::BGRA8Unorm,
        );
        let surfaces_pipeline_state = build_pipeline_state(
            &device,
            &library,
            "surfaces",
            "surface_vertex",
            "surface_fragment",
            MTLPixelFormat::BGRA8Unorm,
        );
        // Replaces the region it covers rather than blending onto it: the
        // shader has already composited the (blurred) destination itself, so
        // source-over would count the sharp original twice.
        let backdrop_blur_pipeline_state = build_path_sprite_pipeline_state(
            &device,
            &library,
            "backdrop_blur",
            "backdrop_blur_vertex",
            "backdrop_blur_fragment",
            MTLPixelFormat::BGRA8Unorm,
        );
        let kawase_down_pipeline_state = build_blit_pipeline_state(
            &device,
            &library,
            "kawase_down",
            "kawase_vertex",
            "kawase_down_fragment",
            MTLPixelFormat::BGRA8Unorm,
        );
        let kawase_up_pipeline_state = build_blit_pipeline_state(
            &device,
            &library,
            "kawase_up",
            "kawase_vertex",
            "kawase_up_fragment",
            MTLPixelFormat::BGRA8Unorm,
        );

        let command_queue = device.new_command_queue();
        let supports_shared_storage = cfg!(target_os = "ios") || is_apple_gpu;
        let sprite_atlas = Arc::new(MetalAtlas::new(device.clone(), supports_shared_storage));
        // metal and objc2-metal expose the same Objective-C device; the borrowed
        // protocol object remains valid while `device` owns it.
        let core_video_texture_cache = unsafe {
            let mut cache = ptr::null_mut();
            let status = CVMetalTextureCache::create(
                None,
                None,
                &*device
                    .as_ptr()
                    .cast::<objc2::runtime::ProtocolObject<dyn objc2_metal::MTLDevice>>(),
                None,
                ptr::NonNull::from(&mut cache),
            );
            assert_eq!(
                status, kCVReturnSuccess,
                "failed to create CoreVideo Metal texture cache"
            );
            CFRetained::from_raw(ptr::NonNull::new(cache).expect("CoreVideo returned a null cache"))
        };

        Self {
            device,
            layer,
            presents_with_transaction: false,
            is_apple_gpu,
            is_unified_memory,
            opaque,
            command_queue,
            paths_rasterization_pipeline_state,
            path_sprites_pipeline_state,
            backdrop_blur_pipeline_state,
            kawase_down_pipeline_state,
            kawase_up_pipeline_state,
            shadows_pipeline_state,
            quads_pipeline_state,
            underlines_pipeline_state,
            monochrome_sprites_pipeline_state,
            polychrome_sprites_pipeline_state,
            surfaces_pipeline_state,
            unit_vertices,
            instance_buffer_pool,
            sprite_atlas,
            core_video_texture_cache,
            path_intermediate_texture: None,
            path_intermediate_msaa_texture: None,
            backdrop_blur_textures: None,
            custom_shaders: CustomShaderResources::new(),
            path_sample_count: PATH_SAMPLE_COUNT,
            #[cfg(any(test, feature = "bench-support", feature = "test-support"))]
            headless_render_target: None,
        }
    }

    pub fn layer(&self) -> Option<&metal::MetalLayerRef> {
        self.layer.as_ref().map(|l| l.as_ref())
    }

    pub fn layer_ptr(&self) -> *mut CAMetalLayer {
        self.layer
            .as_ref()
            .map(|l| l.as_ptr())
            .unwrap_or(ptr::null_mut())
    }

    pub fn sprite_atlas(&self) -> &Arc<MetalAtlas> {
        &self.sprite_atlas
    }

    /// Compile a custom fragment shader and return the handle to paint with.
    ///
    /// `None` on failure (bad WGSL, MSL compile error) — the caller is expected
    /// to fall back to plain primitives, which is also what backends without
    /// custom-shader support return. The reason is logged; returning a `Result`
    /// here would only make every call site log the same line itself, since
    /// `PlatformWindow::register_custom_shader` is `Option`-shaped.
    pub fn register_custom_shader(
        &mut self,
        wgsl_fragment: &str,
        label: &str,
    ) -> Option<CustomShaderId> {
        match self.custom_shaders.register(
            &self.device,
            MTLPixelFormat::BGRA8Unorm,
            wgsl_fragment,
            label,
        ) {
            Ok(id) => Some(id),
            Err(error) => {
                log::error!("failed to register custom shader '{label}': {error:#}");
                None
            }
        }
    }

    pub fn set_presents_with_transaction(&mut self, presents_with_transaction: bool) {
        self.presents_with_transaction = presents_with_transaction;
        if let Some(layer) = &self.layer {
            layer.set_presents_with_transaction(presents_with_transaction);
        }
    }

    pub fn update_drawable_size(&mut self, size: Size<DevicePixels>) {
        if let Some(layer) = &self.layer {
            layer.set_drawable_size(CGSize::new(size.width.0 as f64, size.height.0 as f64));
        }
        self.update_path_intermediate_textures(size);
    }

    fn update_path_intermediate_textures(&mut self, size: Size<DevicePixels>) {
        // We are uncertain when this happens, but sometimes size can be 0 here. Most likely before
        // the layout pass on window creation. Zero-sized texture creation causes SIGABRT.
        // https://github.com/zed-industries/zed/issues/36229
        if size.width.0 <= 0 || size.height.0 <= 0 {
            self.path_intermediate_texture = None;
            self.path_intermediate_msaa_texture = None;
            self.backdrop_blur_textures = None;
            return;
        }

        let texture_descriptor = metal::TextureDescriptor::new();
        texture_descriptor.set_width(size.width.0 as u64);
        texture_descriptor.set_height(size.height.0 as u64);
        texture_descriptor.set_pixel_format(metal::MTLPixelFormat::BGRA8Unorm);
        texture_descriptor.set_storage_mode(metal::MTLStorageMode::Private);
        texture_descriptor
            .set_usage(metal::MTLTextureUsage::RenderTarget | metal::MTLTextureUsage::ShaderRead);
        self.path_intermediate_texture = Some(self.device.new_texture(&texture_descriptor));
        self.update_backdrop_blur_textures(size);

        if self.path_sample_count > 1 {
            // https://developer.apple.com/documentation/metal/choosing-a-resource-storage-mode-for-apple-gpus
            // Rendering MSAA textures are done in a single pass, so we can use memory-less storage on Apple Silicon
            let storage_mode = if self.is_apple_gpu {
                metal::MTLStorageMode::Memoryless
            } else {
                metal::MTLStorageMode::Private
            };

            let msaa_descriptor = texture_descriptor;
            msaa_descriptor.set_texture_type(metal::MTLTextureType::D2Multisample);
            msaa_descriptor.set_storage_mode(storage_mode);
            msaa_descriptor.set_sample_count(self.path_sample_count as _);
            self.path_intermediate_msaa_texture = Some(self.device.new_texture(&msaa_descriptor));
        } else {
            self.path_intermediate_msaa_texture = None;
        }
    }

    pub fn update_transparency(&mut self, transparent: bool) {
        self.opaque = !transparent;
        if let Some(layer) = &self.layer {
            layer.set_opaque(!transparent);
        }
    }

    pub fn destroy(&self) {
        // nothing to do
    }

    pub fn draw(&mut self, scene: &Scene) {
        let layer = match &self.layer {
            Some(l) => l.clone(),
            None => {
                log::error!(
                    "draw() called on headless renderer - use render_scene_to_image() instead"
                );
                return;
            }
        };
        let viewport_size = layer.drawable_size();
        let viewport_size: Size<DevicePixels> = size(
            (viewport_size.width.ceil() as i32).into(),
            (viewport_size.height.ceil() as i32).into(),
        );
        let drawable = if let Some(drawable) = layer.next_drawable() {
            drawable
        } else {
            log::error!(
                "failed to retrieve next drawable, drawable size: {:?}",
                viewport_size
            );
            return;
        };

        let command_buffer = match self.render_frame(scene, drawable.texture(), viewport_size) {
            Ok(command_buffer) => command_buffer,
            Err(error) => {
                log::error!("failed to render: {error:#}");
                return;
            }
        };

        if self.presents_with_transaction {
            command_buffer.commit();
            command_buffer.wait_until_scheduled();
            drawable.present();
        } else {
            command_buffer.present_drawable(drawable);
            command_buffer.commit();
        }
    }

    fn render_frame(
        &mut self,
        scene: &Scene,
        texture: &metal::TextureRef,
        viewport_size: Size<DevicePixels>,
    ) -> Result<metal::CommandBuffer> {
        let mut writer = InstanceBufferWriter::new(
            &self.device,
            &self.instance_buffer_pool,
            self.is_unified_memory,
        );
        let instance_bindings = write_instances(scene, &mut writer).with_context(|| {
            format!(
                "scene too large: {} paths, {} shadows, {} quads, {} underlines, {} mono, {} poly, {} surfaces",
                scene.paths.len(),
                scene.shadows.len(),
                scene.quads.len(),
                scene.underlines.len(),
                scene.monochrome_sprites.len(),
                scene.polychrome_sprites.len(),
                scene.surfaces.len(),
            )
        })?;
        let command_buffer = self.draw_primitives_to_texture(
            scene,
            &instance_bindings,
            &mut writer,
            texture,
            viewport_size,
        )?;

        let instance_buffer_pool = self.instance_buffer_pool.clone();
        let instance_buffer = Cell::new(Some(writer.finish()));
        let block = RcBlock::new(move |_: ptr::NonNull<AnyObject>| {
            if let Some(instance_buffer) = instance_buffer.take() {
                instance_buffer_pool.lock().release(instance_buffer);
            }
        });
        // SAFETY: Both pointee types are opaque views of the same Objective-C block pointer ABI.
        unsafe {
            command_buffer.add_completed_handler(&*RcBlock::as_ptr(&block).cast());
        }

        Ok(command_buffer)
    }

    /// Renders the scene to a texture and returns the pixel data as an RGBA image.
    /// This does not present the frame to screen - useful for visual testing
    /// where we want to capture what would be rendered without displaying it.
    ///
    /// Note: This requires a layer-backed renderer. For headless rendering,
    /// use `render_scene_to_image()` instead.
    pub fn render_to_image(&mut self, scene: &Scene) -> Result<RgbaImage> {
        let layer = self
            .layer
            .clone()
            .ok_or_else(|| anyhow::anyhow!("render_to_image requires a layer-backed renderer"))?;
        let viewport_size = layer.drawable_size();
        let viewport_size: Size<DevicePixels> = size(
            (viewport_size.width.ceil() as i32).into(),
            (viewport_size.height.ceil() as i32).into(),
        );
        let drawable = layer
            .next_drawable()
            .ok_or_else(|| anyhow::anyhow!("Failed to get drawable for render_to_image"))?;

        let command_buffer = self.render_frame(scene, drawable.texture(), viewport_size)?;

        // Commit and wait for completion without presenting
        command_buffer.commit();
        command_buffer.wait_until_completed();

        read_texture_to_image(drawable.texture())
    }

    /// Renders a scene to an image without requiring a window or CAMetalLayer.
    ///
    /// This is the primary method for headless rendering. It creates an offscreen
    /// texture, renders the scene to it, and returns the pixel data as an RGBA image.
    #[cfg(any(test, feature = "bench-support", feature = "test-support"))]
    pub fn render_scene_to_image(
        &mut self,
        scene: &Scene,
        size: Size<DevicePixels>,
    ) -> Result<RgbaImage> {
        if size.width.0 <= 0 || size.height.0 <= 0 {
            anyhow::bail!("Invalid size for render_scene_to_image: {:?}", size);
        }

        // Headless callers do not have a Cocoa event-loop pool to release
        // autoreleased command buffers and render-pass descriptors.
        objc2::rc::autoreleasepool(|_| {
            // Update path intermediate textures for this size
            self.update_path_intermediate_textures(size);

            // Create an offscreen texture as render target
            let texture_descriptor = metal::TextureDescriptor::new();
            texture_descriptor.set_width(size.width.0 as u64);
            texture_descriptor.set_height(size.height.0 as u64);
            texture_descriptor.set_pixel_format(MTLPixelFormat::BGRA8Unorm);
            texture_descriptor.set_usage(
                metal::MTLTextureUsage::RenderTarget | metal::MTLTextureUsage::ShaderRead,
            );
            // Like the atlas, only Apple GPUs can create shared textures on macOS;
            // Intel Macs cannot, even with unified memory. iOS has no managed storage.
            let uses_shared_storage = cfg!(target_os = "ios") || self.is_apple_gpu;
            texture_descriptor.set_storage_mode(if uses_shared_storage {
                metal::MTLStorageMode::Shared
            } else {
                metal::MTLStorageMode::Managed
            });
            let target_texture = self.device.new_texture(&texture_descriptor);

            let command_buffer = self.render_frame(scene, &target_texture, size)?;

            // Managed textures require an explicit blit synchronize before the CPU
            // can read back the rendered data. Without this, get_bytes returns
            // stale zeros.
            if !uses_shared_storage {
                let blit = command_buffer.new_blit_command_encoder();
                blit.synchronize_resource(&target_texture);
                blit.end_encoding();
            }

            // Commit and wait for completion
            command_buffer.commit();
            command_buffer.wait_until_completed();

            read_texture_to_image(&target_texture)
        })
    }

    /// Renders a scene to a reused offscreen texture without reading pixels
    /// back or blocking on GPU completion.
    ///
    /// This mirrors the CPU cost of presenting a frame to a window (scene
    /// encoding, instance buffer writes, command submission) and is used by
    /// headless benchmark rendering, where the produced pixels are never
    /// inspected.
    #[cfg(any(test, feature = "bench-support", feature = "test-support"))]
    pub fn render_scene(&mut self, scene: &Scene, size: Size<DevicePixels>) -> Result<()> {
        if size.width.0 <= 0 || size.height.0 <= 0 {
            anyhow::bail!("Invalid size for render_scene: {:?}", size);
        }

        objc2::rc::autoreleasepool(|_| {
            self.update_path_intermediate_textures(size);

            let needs_new_target = self.headless_render_target.as_ref().is_none_or(|texture| {
                texture.width() != size.width.0 as u64 || texture.height() != size.height.0 as u64
            });
            if needs_new_target {
                let texture_descriptor = metal::TextureDescriptor::new();
                texture_descriptor.set_width(size.width.0 as u64);
                texture_descriptor.set_height(size.height.0 as u64);
                texture_descriptor.set_pixel_format(MTLPixelFormat::BGRA8Unorm);
                texture_descriptor.set_usage(
                    metal::MTLTextureUsage::RenderTarget | metal::MTLTextureUsage::ShaderRead,
                );
                texture_descriptor.set_storage_mode(metal::MTLStorageMode::Private);
                self.headless_render_target = Some(self.device.new_texture(&texture_descriptor));
            }
            let target_texture = self
                .headless_render_target
                .clone()
                .expect("just ensured the render target exists");

            let command_buffer = self.render_frame(scene, &target_texture, size)?;

            // Commit without waiting, mirroring presentation to a real window where
            // the CPU doesn't block on the GPU.
            command_buffer.commit();
            Ok(())
        })
    }

    fn draw_primitives_to_texture(
        &mut self,
        scene: &Scene,
        instance_bindings: &InstanceBindings,
        writer: &mut InstanceBufferWriter,
        texture: &metal::TextureRef,
        viewport_size: Size<DevicePixels>,
    ) -> Result<metal::CommandBuffer> {
        let command_queue = self.command_queue.clone();
        let command_buffer = command_queue.new_command_buffer();
        let alpha = if self.opaque { 1. } else { 0. };

        let mut command_encoder = new_command_encoder_for_texture(
            command_buffer,
            texture,
            viewport_size,
            Some(metal::MTLClearColor::new(0., 0., 0., alpha)),
        );

        for batch in scene.batches() {
            match batch {
                PrimitiveBatch::Shadows(range) => {
                    self.draw_shadows(range, instance_bindings, viewport_size, command_encoder)
                }
                PrimitiveBatch::Quads(range) => {
                    self.draw_quads(range, instance_bindings, viewport_size, command_encoder)
                }
                PrimitiveBatch::Paths(range) => {
                    let paths = &scene.paths[range];
                    command_encoder.end_encoding();

                    let did_draw = self.draw_paths_to_intermediate(
                        paths,
                        writer,
                        viewport_size,
                        command_buffer,
                    )?;

                    command_encoder = new_command_encoder_for_texture(
                        command_buffer,
                        texture,
                        viewport_size,
                        None,
                    );

                    if did_draw {
                        if let Err(error) = self.draw_paths_from_intermediate(
                            paths,
                            writer,
                            viewport_size,
                            command_encoder,
                        ) {
                            command_encoder.end_encoding();
                            return Err(error);
                        }
                    }
                }
                PrimitiveBatch::Underlines(range) => {
                    self.draw_underlines(range, instance_bindings, viewport_size, command_encoder)
                }
                PrimitiveBatch::MonochromeSprites { texture_id, range } => self
                    .draw_monochrome_sprites(
                        texture_id,
                        range,
                        instance_bindings,
                        viewport_size,
                        command_encoder,
                    ),
                PrimitiveBatch::PolychromeSprites { texture_id, range } => self
                    .draw_polychrome_sprites(
                        texture_id,
                        range,
                        instance_bindings,
                        viewport_size,
                        command_encoder,
                    ),
                PrimitiveBatch::Surfaces(range) => self.draw_surfaces(
                    &scene.surfaces[range.clone()],
                    range.start,
                    instance_bindings,
                    viewport_size,
                    command_encoder,
                ),
                PrimitiveBatch::SubpixelSprites { .. } => unreachable!(),
                PrimitiveBatch::CustomShaders { shader_id, range } => self.draw_custom_shaders(
                    shader_id,
                    range,
                    scene.custom_shaders.len(),
                    instance_bindings,
                    viewport_size,
                    command_encoder,
                ),
                PrimitiveBatch::BackdropBlurs(range) => {
                    let blurs = &scene.backdrop_blurs[range.clone()];
                    // A backdrop blur reads the target it is being drawn into,
                    // so the pass has to be resolved before it can sample.
                    // Same shape as the Paths arm above; the difference is the
                    // direction — that one renders *into* an intermediate,
                    // this one filters the target *out of* it.
                    command_encoder.end_encoding();

                    let radius = blurs.first().map_or(0., |blur| blur.blur_radius);
                    let blurred = self
                        .blur_backdrop(texture, blurs, radius, command_buffer)
                        .map(|blurred| blurred.to_owned());

                    // `None` = keep what the pass already painted
                    // (`MTLLoadAction::Load`), the same resume the Paths arm
                    // above needs. Clearing here would throw away everything
                    // drawn before the blur.
                    command_encoder = new_command_encoder_for_texture(
                        command_buffer,
                        texture,
                        viewport_size,
                        None,
                    );

                    // No scratch textures (zero-sized drawable): skip the
                    // effect rather than dropping the frame.
                    if let Some(blurred) = blurred {
                        self.draw_backdrop_blurs(
                            range,
                            &blurred,
                            instance_bindings,
                            viewport_size,
                            command_encoder,
                        );
                    }
                }
            }
        }

        command_encoder.end_encoding();

        Ok(command_buffer.to_owned())
    }

    /// (Re)allocate the half-resolution ping-pong pair used by the backdrop
    /// blur. Called from `update_path_intermediate_textures`, so the pair
    /// tracks the drawable exactly like the path intermediate does.
    fn update_backdrop_blur_textures(&mut self, size: Size<DevicePixels>) {
        let width = (size.width.0 / 2).max(1) as u64;
        let height = (size.height.0 / 2).max(1) as u64;

        let make = || {
            let descriptor = metal::TextureDescriptor::new();
            descriptor.set_width(width);
            descriptor.set_height(height);
            descriptor.set_pixel_format(metal::MTLPixelFormat::BGRA8Unorm);
            descriptor.set_storage_mode(metal::MTLStorageMode::Private);
            descriptor.set_usage(
                metal::MTLTextureUsage::RenderTarget | metal::MTLTextureUsage::ShaderRead,
            );
            self.device.new_texture(&descriptor)
        };
        self.backdrop_blur_textures = Some([make(), make()]);
    }

    /// Run one dual-Kawase pass from `source` into `target`.
    fn kawase_pass(
        &self,
        pipeline: &metal::RenderPipelineStateRef,
        source: &metal::TextureRef,
        target: &metal::TextureRef,
        texel: [f32; 2],
        offset: f32,
        // Region to shade, normalised: (x, y, width, height).
        region: (f32, f32, f32, f32),
        command_buffer: &metal::CommandBufferRef,
    ) {
        let descriptor = metal::RenderPassDescriptor::new();
        let color_attachment = descriptor.color_attachments().object_at(0).unwrap();
        color_attachment.set_texture(Some(target));
        // Every texel is written by the fullscreen triangle pair, so the
        // previous contents are irrelevant — and not loading them saves the
        // tile-memory fill on Apple GPUs.
        color_attachment.set_load_action(metal::MTLLoadAction::DontCare);
        color_attachment.set_store_action(metal::MTLStoreAction::Store);

        let encoder = command_buffer.new_render_command_encoder(descriptor);
        encoder.set_render_pipeline_state(pipeline);
        encoder.set_viewport(metal::MTLViewport {
            originX: 0.0,
            originY: 0.0,
            width: target.width() as f64,
            height: target.height() as f64,
            znear: 0.0,
            zfar: 1.0,
        });
        encoder.set_vertex_buffer(
            KawaseInputIndex::Vertices as u64,
            Some(&self.unit_vertices),
            0,
        );
        let params = KawaseParams {
            texel,
            offset,
            _pad: 0.0,
            origin: [region.0, region.1],
            size: [region.2, region.3],
        };
        // The vertex stage needs the region too — it is what shrinks the quad.
        encoder.set_vertex_bytes(
            KawaseInputIndex::Params as u64,
            mem::size_of::<KawaseParams>() as u64,
            &params as *const KawaseParams as *const _,
        );
        encoder.set_fragment_bytes(
            KawaseInputIndex::Params as u64,
            mem::size_of::<KawaseParams>() as u64,
            &params as *const KawaseParams as *const _,
        );
        encoder.set_fragment_texture(KawaseInputIndex::SourceTexture as u64, Some(source));
        encoder.draw_primitives(metal::MTLPrimitiveType::Triangle, 0, 6);
        encoder.end_encoding();
    }

    /// Blur the frame painted so far into the scratch chain and return the
    /// texture holding the result.
    ///
    /// Must be called with no render encoder open on `source`: this samples
    /// the very target the scene is being drawn into, so those writes have to
    /// be resolved first. The caller reopens its encoder with
    /// `MTLLoadAction::Load` afterwards.
    fn blur_backdrop(
        &self,
        source: &metal::TextureRef,
        blurs: &[BackdropBlur],
        blur_radius: f32,
        command_buffer: &metal::CommandBufferRef,
    ) -> Option<&metal::Texture> {
        let textures = self.backdrop_blur_textures.as_ref()?;
        let (half_width, half_height) = (textures[0].width(), textures[0].height());
        if source.width() == 0 || source.height() == 0 {
            return None;
        }

        let region = blur_region(blurs, blur_radius, source.width(), source.height())?;

        // Downsample the target into half res, over the blur region only.
        self.kawase_pass(
            &self.kawase_down_pipeline_state,
            source,
            &textures[0],
            [1.0 / source.width() as f32, 1.0 / source.height() as f32],
            1.0,
            region,
            command_buffer,
        );

        // Then ping-pong at half res with a growing offset.
        let half_texel = [1.0 / half_width as f32, 1.0 / half_height as f32];
        let mut read = 0usize;
        for offset in kawase_offsets(blur_radius * 0.5) {
            let write = 1 - read;
            self.kawase_pass(
                &self.kawase_up_pipeline_state,
                &textures[read],
                &textures[write],
                half_texel,
                offset,
                region,
                command_buffer,
            );
            read = write;
        }
        Some(&textures[read])
    }

    fn draw_backdrop_blurs(
        &self,
        blurs: Range<usize>,
        blurred: &metal::TextureRef,
        instance_bindings: &InstanceBindings,
        viewport_size: Size<DevicePixels>,
        command_encoder: &metal::RenderCommandEncoderRef,
    ) {
        if blurs.is_empty() {
            return;
        }

        command_encoder.set_render_pipeline_state(&self.backdrop_blur_pipeline_state);
        command_encoder.set_vertex_buffer(
            BackdropBlurInputIndex::Vertices as u64,
            Some(&self.unit_vertices),
            0,
        );
        command_encoder.set_vertex_buffer(
            BackdropBlurInputIndex::BackdropBlurs as u64,
            Some(&instance_bindings.backdrop_blurs.buffer),
            instance_bindings.backdrop_blurs.offset as u64,
        );
        command_encoder.set_fragment_buffer(
            BackdropBlurInputIndex::BackdropBlurs as u64,
            Some(&instance_bindings.backdrop_blurs.buffer),
            instance_bindings.backdrop_blurs.offset as u64,
        );
        command_encoder.set_vertex_bytes(
            BackdropBlurInputIndex::ViewportSize as u64,
            mem::size_of_val(&viewport_size) as u64,
            &viewport_size as *const Size<DevicePixels> as *const _,
        );
        command_encoder.set_fragment_bytes(
            BackdropBlurInputIndex::ViewportSize as u64,
            mem::size_of_val(&viewport_size) as u64,
            &viewport_size as *const Size<DevicePixels> as *const _,
        );
        command_encoder
            .set_fragment_texture(BackdropBlurInputIndex::BlurredTexture as u64, Some(blurred));

        // The buffer is bound at the base of the whole `scene.backdrop_blurs`
        // array (like every other primitive type since the 2026-08-04 merge),
        // so the batch must start at its range offset. Plain
        // `draw_primitives_instanced` re-drew instance 0 for every batch: with
        // two glass surfaces open (dropdown + flyout), the second batch painted
        // the first surface's bounds/tint again — a flickering, wrongly
        // coloured rectangle.
        command_encoder.draw_primitives_instanced_base_instance(
            metal::MTLPrimitiveType::Triangle,
            0,
            6,
            blurs.len() as u64,
            blurs.start as u64,
        );
    }

    fn draw_paths_to_intermediate(
        &self,
        paths: &[Path<ScaledPixels>],
        writer: &mut InstanceBufferWriter,
        viewport_size: Size<DevicePixels>,
        command_buffer: &metal::CommandBufferRef,
    ) -> Result<bool> {
        if paths.is_empty() {
            return Ok(false);
        }
        let intermediate_texture = self
            .path_intermediate_texture
            .as_ref()
            .context("missing path intermediate texture")?;

        let mut vertices = Vec::new();
        for path in paths {
            vertices.extend(path.vertices.iter().map(|v| PathRasterizationVertex {
                xy_position: v.xy_position,
                st_position: v.st_position,
                color: path.color,
                bounds: path.bounds.intersect(&path.content_mask.bounds),
            }));
        }
        let vertex_instance_bindings = writer.write(&vertices)?;

        let render_pass_descriptor = metal::RenderPassDescriptor::new();
        let color_attachment = render_pass_descriptor
            .color_attachments()
            .object_at(0)
            .unwrap();
        color_attachment.set_load_action(metal::MTLLoadAction::Clear);
        color_attachment.set_clear_color(metal::MTLClearColor::new(0., 0., 0., 0.));

        if let Some(msaa_texture) = &self.path_intermediate_msaa_texture {
            color_attachment.set_texture(Some(msaa_texture));
            color_attachment.set_resolve_texture(Some(intermediate_texture));
            color_attachment.set_store_action(metal::MTLStoreAction::MultisampleResolve);
        } else {
            color_attachment.set_texture(Some(intermediate_texture));
            color_attachment.set_store_action(metal::MTLStoreAction::Store);
        }

        let command_encoder = command_buffer.new_render_command_encoder(render_pass_descriptor);
        command_encoder.set_render_pipeline_state(&self.paths_rasterization_pipeline_state);
        command_encoder.set_vertex_buffer(
            PathRasterizationInputIndex::Vertices as u64,
            Some(&vertex_instance_bindings.buffer),
            vertex_instance_bindings.offset as u64,
        );
        command_encoder.set_vertex_bytes(
            PathRasterizationInputIndex::ViewportSize as u64,
            mem::size_of_val(&viewport_size) as u64,
            &viewport_size as *const Size<DevicePixels> as *const _,
        );
        command_encoder.set_fragment_buffer(
            PathRasterizationInputIndex::Vertices as u64,
            Some(&vertex_instance_bindings.buffer),
            vertex_instance_bindings.offset as u64,
        );
        command_encoder.draw_primitives(
            metal::MTLPrimitiveType::Triangle,
            0,
            vertices.len() as u64,
        );

        command_encoder.end_encoding();
        Ok(true)
    }

    fn draw_shadows(
        &self,
        shadows: Range<usize>,
        instance_bindings: &InstanceBindings,
        viewport_size: Size<DevicePixels>,
        command_encoder: &metal::RenderCommandEncoderRef,
    ) {
        if shadows.is_empty() {
            return;
        }

        command_encoder.set_render_pipeline_state(&self.shadows_pipeline_state);
        command_encoder.set_vertex_buffer(
            ShadowInputIndex::Vertices as u64,
            Some(&self.unit_vertices),
            0,
        );
        command_encoder.set_vertex_buffer(
            ShadowInputIndex::Shadows as u64,
            Some(&instance_bindings.shadows.buffer),
            instance_bindings.shadows.offset as u64,
        );
        command_encoder.set_fragment_buffer(
            ShadowInputIndex::Shadows as u64,
            Some(&instance_bindings.shadows.buffer),
            instance_bindings.shadows.offset as u64,
        );
        command_encoder.set_vertex_bytes(
            ShadowInputIndex::ViewportSize as u64,
            mem::size_of_val(&viewport_size) as u64,
            &viewport_size as *const Size<DevicePixels> as *const _,
        );

        command_encoder.draw_primitives_instanced_base_instance(
            metal::MTLPrimitiveType::Triangle,
            0,
            6,
            shadows.len() as u64,
            shadows.start as u64,
        );
    }

    fn draw_quads(
        &self,
        quads: Range<usize>,
        instance_bindings: &InstanceBindings,
        viewport_size: Size<DevicePixels>,
        command_encoder: &metal::RenderCommandEncoderRef,
    ) {
        if quads.is_empty() {
            return;
        }

        command_encoder.set_render_pipeline_state(&self.quads_pipeline_state);
        command_encoder.set_vertex_buffer(
            QuadInputIndex::Vertices as u64,
            Some(&self.unit_vertices),
            0,
        );
        command_encoder.set_vertex_buffer(
            QuadInputIndex::Quads as u64,
            Some(&instance_bindings.quads.buffer),
            instance_bindings.quads.offset as u64,
        );
        command_encoder.set_fragment_buffer(
            QuadInputIndex::Quads as u64,
            Some(&instance_bindings.quads.buffer),
            instance_bindings.quads.offset as u64,
        );
        command_encoder.set_vertex_bytes(
            QuadInputIndex::ViewportSize as u64,
            mem::size_of_val(&viewport_size) as u64,
            &viewport_size as *const Size<DevicePixels> as *const _,
        );

        command_encoder.draw_primitives_instanced_base_instance(
            metal::MTLPrimitiveType::Triangle,
            0,
            6,
            quads.len() as u64,
            quads.start as u64,
        );
    }

    /// Draw one batch of custom-shader quads.
    ///
    /// Unlike the built-in pipelines this one binds no vertex buffer: the WGSL
    /// template builds its unit quad from `[[vertex_id]]` as a four-vertex
    /// triangle strip, so the geometry is the same shape the wgpu and DirectX
    /// backends draw for the identical shader source.
    ///
    /// An unknown `shader_id` is skipped rather than reported: it can only come
    /// from a handle this renderer never issued, and dropping the frame over a
    /// decorative primitive would be the worse failure.
    fn draw_custom_shaders(
        &self,
        shader_id: CustomShaderId,
        range: Range<usize>,
        instance_count: usize,
        instance_bindings: &InstanceBindings,
        viewport_size: Size<DevicePixels>,
        command_encoder: &metal::RenderCommandEncoderRef,
    ) {
        if range.is_empty() {
            return;
        }
        let Some(pipeline) = self.custom_shaders.pipeline(shader_id) else {
            return;
        };

        let globals = CustomShaderGlobals {
            viewport_size: [
                i32::from(viewport_size.width) as f32,
                i32::from(viewport_size.height) as f32,
            ],
        };
        // naga's `_mslBufferSizes`, in bytes, for the whole bound array — not
        // just this batch's slice, since the binding starts at element 0.
        let instances_byte_length =
            [(instance_count * mem::size_of::<gpui::CustomShaderInstance>()) as u32];

        command_encoder.set_render_pipeline_state(pipeline);
        command_encoder.set_vertex_buffer(
            CustomShaderInputIndex::Instances as u64,
            Some(&instance_bindings.custom_shaders.buffer),
            instance_bindings.custom_shaders.offset as u64,
        );
        command_encoder.set_fragment_buffer(
            CustomShaderInputIndex::Instances as u64,
            Some(&instance_bindings.custom_shaders.buffer),
            instance_bindings.custom_shaders.offset as u64,
        );
        // Both stages: the vertex stage needs it for the NDC transform, and a
        // `custom_effect` is free to read `globals` too.
        command_encoder.set_vertex_bytes(
            CustomShaderInputIndex::ViewportSize as u64,
            mem::size_of_val(&globals) as u64,
            &globals as *const CustomShaderGlobals as *const _,
        );
        command_encoder.set_fragment_bytes(
            CustomShaderInputIndex::ViewportSize as u64,
            mem::size_of_val(&globals) as u64,
            &globals as *const CustomShaderGlobals as *const _,
        );
        command_encoder.set_vertex_bytes(
            CustomShaderInputIndex::InstancesByteLength as u64,
            mem::size_of_val(&instances_byte_length) as u64,
            instances_byte_length.as_ptr() as *const _,
        );
        command_encoder.set_fragment_bytes(
            CustomShaderInputIndex::InstancesByteLength as u64,
            mem::size_of_val(&instances_byte_length) as u64,
            instances_byte_length.as_ptr() as *const _,
        );

        // `[[instance_id]]` on Metal counts from the base instance, so the
        // whole array stays bound and the batch's sub-range is expressed as the
        // base instance — same as `draw_quads`.
        command_encoder.draw_primitives_instanced_base_instance(
            metal::MTLPrimitiveType::TriangleStrip,
            0,
            4,
            range.len() as u64,
            range.start as u64,
        );
    }

    fn draw_paths_from_intermediate(
        &self,
        paths: &[Path<ScaledPixels>],
        writer: &mut InstanceBufferWriter,
        viewport_size: Size<DevicePixels>,
        command_encoder: &metal::RenderCommandEncoderRef,
    ) -> Result<()> {
        let Some(first_path) = paths.first() else {
            return Ok(());
        };
        let intermediate_texture = self
            .path_intermediate_texture
            .as_ref()
            .context("missing path intermediate texture")?;

        command_encoder.set_render_pipeline_state(&self.path_sprites_pipeline_state);
        command_encoder.set_vertex_buffer(
            SpriteInputIndex::Vertices as u64,
            Some(&self.unit_vertices),
            0,
        );
        command_encoder.set_vertex_bytes(
            SpriteInputIndex::ViewportSize as u64,
            mem::size_of_val(&viewport_size) as u64,
            &viewport_size as *const Size<DevicePixels> as *const _,
        );

        command_encoder.set_fragment_texture(
            SpriteInputIndex::AtlasTexture as u64,
            Some(intermediate_texture),
        );

        // When copying paths from the intermediate texture to the drawable,
        // each pixel must only be copied once, in case of transparent paths.
        //
        // If all paths have the same draw order, then their bounds are all
        // disjoint, so we can copy each path's bounds individually. If this
        // batch combines different draw orders, we perform a single copy
        // for a minimal spanning rect.
        let sprites;
        if paths.last().unwrap().order == first_path.order {
            sprites = paths
                .iter()
                .map(|path| PathSprite {
                    bounds: path.clipped_bounds(),
                })
                .collect();
        } else {
            let mut bounds = first_path.clipped_bounds();
            for path in paths.iter().skip(1) {
                bounds = bounds.union(&path.clipped_bounds());
            }
            sprites = vec![PathSprite { bounds }];
        }

        let sprite_instance_bindings = writer.write(&sprites)?;
        command_encoder.set_vertex_buffer(
            SpriteInputIndex::Sprites as u64,
            Some(&sprite_instance_bindings.buffer),
            sprite_instance_bindings.offset as u64,
        );

        command_encoder.draw_primitives_instanced(
            metal::MTLPrimitiveType::Triangle,
            0,
            6,
            sprites.len() as u64,
        );
        Ok(())
    }

    fn draw_underlines(
        &self,
        underlines: Range<usize>,
        instance_bindings: &InstanceBindings,
        viewport_size: Size<DevicePixels>,
        command_encoder: &metal::RenderCommandEncoderRef,
    ) {
        if underlines.is_empty() {
            return;
        }

        command_encoder.set_render_pipeline_state(&self.underlines_pipeline_state);
        command_encoder.set_vertex_buffer(
            UnderlineInputIndex::Vertices as u64,
            Some(&self.unit_vertices),
            0,
        );
        command_encoder.set_vertex_buffer(
            UnderlineInputIndex::Underlines as u64,
            Some(&instance_bindings.underlines.buffer),
            instance_bindings.underlines.offset as u64,
        );
        command_encoder.set_fragment_buffer(
            UnderlineInputIndex::Underlines as u64,
            Some(&instance_bindings.underlines.buffer),
            instance_bindings.underlines.offset as u64,
        );
        command_encoder.set_vertex_bytes(
            UnderlineInputIndex::ViewportSize as u64,
            mem::size_of_val(&viewport_size) as u64,
            &viewport_size as *const Size<DevicePixels> as *const _,
        );

        command_encoder.draw_primitives_instanced_base_instance(
            metal::MTLPrimitiveType::Triangle,
            0,
            6,
            underlines.len() as u64,
            underlines.start as u64,
        );
    }

    fn draw_monochrome_sprites(
        &self,
        texture_id: AtlasTextureId,
        sprites: Range<usize>,
        instance_bindings: &InstanceBindings,
        viewport_size: Size<DevicePixels>,
        command_encoder: &metal::RenderCommandEncoderRef,
    ) {
        if sprites.is_empty() {
            return;
        }

        let Some(texture) = self.sprite_atlas.metal_texture(texture_id) else {
            return;
        };
        let texture_size = size(
            DevicePixels(texture.width() as i32),
            DevicePixels(texture.height() as i32),
        );
        command_encoder.set_render_pipeline_state(&self.monochrome_sprites_pipeline_state);
        command_encoder.set_vertex_buffer(
            SpriteInputIndex::Vertices as u64,
            Some(&self.unit_vertices),
            0,
        );
        command_encoder.set_vertex_buffer(
            SpriteInputIndex::Sprites as u64,
            Some(&instance_bindings.monochrome_sprites.buffer),
            instance_bindings.monochrome_sprites.offset as u64,
        );
        command_encoder.set_vertex_bytes(
            SpriteInputIndex::ViewportSize as u64,
            mem::size_of_val(&viewport_size) as u64,
            &viewport_size as *const Size<DevicePixels> as *const _,
        );
        command_encoder.set_vertex_bytes(
            SpriteInputIndex::AtlasTextureSize as u64,
            mem::size_of_val(&texture_size) as u64,
            &texture_size as *const Size<DevicePixels> as *const _,
        );
        command_encoder.set_fragment_buffer(
            SpriteInputIndex::Sprites as u64,
            Some(&instance_bindings.monochrome_sprites.buffer),
            instance_bindings.monochrome_sprites.offset as u64,
        );
        command_encoder.set_fragment_texture(SpriteInputIndex::AtlasTexture as u64, Some(&texture));

        command_encoder.draw_primitives_instanced_base_instance(
            metal::MTLPrimitiveType::Triangle,
            0,
            6,
            sprites.len() as u64,
            sprites.start as u64,
        );
    }

    fn draw_polychrome_sprites(
        &self,
        texture_id: AtlasTextureId,
        sprites: Range<usize>,
        instance_bindings: &InstanceBindings,
        viewport_size: Size<DevicePixels>,
        command_encoder: &metal::RenderCommandEncoderRef,
    ) {
        if sprites.is_empty() {
            return;
        }

        let Some(texture) = self.sprite_atlas.metal_texture(texture_id) else {
            return;
        };
        let texture_size = size(
            DevicePixels(texture.width() as i32),
            DevicePixels(texture.height() as i32),
        );
        command_encoder.set_render_pipeline_state(&self.polychrome_sprites_pipeline_state);
        command_encoder.set_vertex_buffer(
            SpriteInputIndex::Vertices as u64,
            Some(&self.unit_vertices),
            0,
        );
        command_encoder.set_vertex_buffer(
            SpriteInputIndex::Sprites as u64,
            Some(&instance_bindings.polychrome_sprites.buffer),
            instance_bindings.polychrome_sprites.offset as u64,
        );
        command_encoder.set_vertex_bytes(
            SpriteInputIndex::ViewportSize as u64,
            mem::size_of_val(&viewport_size) as u64,
            &viewport_size as *const Size<DevicePixels> as *const _,
        );
        command_encoder.set_vertex_bytes(
            SpriteInputIndex::AtlasTextureSize as u64,
            mem::size_of_val(&texture_size) as u64,
            &texture_size as *const Size<DevicePixels> as *const _,
        );
        command_encoder.set_fragment_buffer(
            SpriteInputIndex::Sprites as u64,
            Some(&instance_bindings.polychrome_sprites.buffer),
            instance_bindings.polychrome_sprites.offset as u64,
        );
        command_encoder.set_fragment_texture(SpriteInputIndex::AtlasTexture as u64, Some(&texture));

        command_encoder.draw_primitives_instanced_base_instance(
            metal::MTLPrimitiveType::Triangle,
            0,
            6,
            sprites.len() as u64,
            sprites.start as u64,
        );
    }

    fn draw_surfaces(
        &mut self,
        surfaces: &[PaintSurface],
        first_surface: usize,
        instance_bindings: &InstanceBindings,
        viewport_size: Size<DevicePixels>,
        command_encoder: &metal::RenderCommandEncoderRef,
    ) {
        if surfaces.is_empty() {
            return;
        }

        command_encoder.set_render_pipeline_state(&self.surfaces_pipeline_state);
        command_encoder.set_vertex_buffer(
            SurfaceInputIndex::Vertices as u64,
            Some(&self.unit_vertices),
            0,
        );
        command_encoder.set_vertex_buffer(
            SurfaceInputIndex::Surfaces as u64,
            Some(&instance_bindings.surfaces.buffer),
            instance_bindings.surfaces.offset as u64,
        );
        command_encoder.set_vertex_bytes(
            SurfaceInputIndex::ViewportSize as u64,
            mem::size_of_val(&viewport_size) as u64,
            &viewport_size as *const Size<DevicePixels> as *const _,
        );

        for (index, surface) in surfaces.iter().enumerate() {
            let image_buffer = &surface.image_buffer;
            let texture_size = size(
                DevicePixels::from(CVPixelBufferGetWidth(image_buffer) as i32),
                DevicePixels::from(CVPixelBufferGetHeight(image_buffer) as i32),
            );

            assert_eq!(
                CVPixelBufferGetPixelFormatType(image_buffer),
                kCVPixelFormatType_420YpCbCr8BiPlanarFullRange
            );

            let plane_texture = |plane, format| unsafe {
                let mut texture = ptr::null_mut();
                let status = CVMetalTextureCache::create_texture_from_image(
                    None,
                    &self.core_video_texture_cache,
                    image_buffer,
                    None,
                    format,
                    CVPixelBufferGetWidthOfPlane(image_buffer, plane),
                    CVPixelBufferGetHeightOfPlane(image_buffer, plane),
                    plane,
                    ptr::NonNull::from(&mut texture),
                );
                assert_eq!(
                    status, kCVReturnSuccess,
                    "failed to create CoreVideo Metal texture"
                );
                CFRetained::<CVMetalTexture>::from_raw(
                    ptr::NonNull::new(texture).expect("CoreVideo returned a null texture"),
                )
            };
            let y_texture = plane_texture(0, objc2_metal::MTLPixelFormat::R8Unorm);
            let cb_cr_texture = plane_texture(1, objc2_metal::MTLPixelFormat::RG8Unorm);

            command_encoder.set_vertex_bytes(
                SurfaceInputIndex::TextureSize as u64,
                mem::size_of_val(&texture_size) as u64,
                &texture_size as *const Size<DevicePixels> as *const _,
            );
            command_encoder.set_fragment_texture(SurfaceInputIndex::YTexture as u64, unsafe {
                let texture = CVMetalTextureGetTexture(&y_texture)
                    .expect("CoreVideo returned no Metal texture");
                Some(metal::TextureRef::from_ptr(
                    objc2::rc::Retained::as_ptr(&texture) as *mut _,
                ))
            });
            command_encoder.set_fragment_texture(SurfaceInputIndex::CbCrTexture as u64, unsafe {
                let texture = CVMetalTextureGetTexture(&cb_cr_texture)
                    .expect("CoreVideo returned no Metal texture");
                Some(metal::TextureRef::from_ptr(
                    objc2::rc::Retained::as_ptr(&texture) as *mut _,
                ))
            });

            command_encoder.draw_primitives_instanced_base_instance(
                metal::MTLPrimitiveType::Triangle,
                0,
                6,
                1,
                (first_surface + index) as u64,
            );
        }
    }
}

fn new_command_encoder_for_texture<'a>(
    command_buffer: &'a metal::CommandBufferRef,
    texture: &'a metal::TextureRef,
    viewport_size: Size<DevicePixels>,
    clear_color: Option<metal::MTLClearColor>,
) -> &'a metal::RenderCommandEncoderRef {
    let render_pass_descriptor = metal::RenderPassDescriptor::new();
    let color_attachment = render_pass_descriptor
        .color_attachments()
        .object_at(0)
        .unwrap();
    color_attachment.set_texture(Some(texture));
    color_attachment.set_store_action(metal::MTLStoreAction::Store);
    if let Some(clear_color) = clear_color {
        color_attachment.set_load_action(metal::MTLLoadAction::Clear);
        color_attachment.set_clear_color(clear_color);
    } else {
        color_attachment.set_load_action(metal::MTLLoadAction::Load);
    }

    let command_encoder = command_buffer.new_render_command_encoder(render_pass_descriptor);
    command_encoder.set_viewport(metal::MTLViewport {
        originX: 0.0,
        originY: 0.0,
        width: i32::from(viewport_size.width) as f64,
        height: i32::from(viewport_size.height) as f64,
        znear: 0.0,
        zfar: 1.0,
    });
    command_encoder
}

// Not gated on `test-support`, unlike upstream: `render_to_image` above is
// ungated in this fork (commit c4724161) so the inspector can screenshot a
// live window, and it reads its pixels back through here.
fn read_texture_to_image(texture: &metal::TextureRef) -> Result<RgbaImage> {
    let width = texture.width() as u32;
    let height = texture.height() as u32;
    let bytes_per_row = width as usize * 4;
    let mut pixels = vec![0u8; height as usize * bytes_per_row];

    let region = metal::MTLRegion {
        origin: metal::MTLOrigin { x: 0, y: 0, z: 0 },
        size: metal::MTLSize {
            width: width as u64,
            height: height as u64,
            depth: 1,
        },
    };
    texture.get_bytes(
        pixels.as_mut_ptr() as *mut std::ffi::c_void,
        bytes_per_row as u64,
        region,
        0,
    );

    // Convert BGRA to RGBA (swap B and R channels)
    for chunk in pixels.chunks_exact_mut(4) {
        chunk.swap(0, 2);
    }

    RgbaImage::from_raw(width, height, pixels).context("failed to create RgbaImage from pixel data")
}

fn build_pipeline_state(
    device: &metal::DeviceRef,
    library: &metal::LibraryRef,
    label: &str,
    vertex_fn_name: &str,
    fragment_fn_name: &str,
    pixel_format: metal::MTLPixelFormat,
) -> metal::RenderPipelineState {
    let vertex_fn = library
        .get_function(vertex_fn_name, None)
        .expect("error locating vertex function");
    let fragment_fn = library
        .get_function(fragment_fn_name, None)
        .expect("error locating fragment function");

    let descriptor = metal::RenderPipelineDescriptor::new();
    descriptor.set_label(label);
    descriptor.set_vertex_function(Some(vertex_fn.as_ref()));
    descriptor.set_fragment_function(Some(fragment_fn.as_ref()));
    let color_attachment = descriptor.color_attachments().object_at(0).unwrap();
    color_attachment.set_pixel_format(pixel_format);
    color_attachment.set_blending_enabled(true);
    color_attachment.set_rgb_blend_operation(metal::MTLBlendOperation::Add);
    color_attachment.set_alpha_blend_operation(metal::MTLBlendOperation::Add);
    color_attachment.set_source_rgb_blend_factor(metal::MTLBlendFactor::SourceAlpha);
    color_attachment.set_source_alpha_blend_factor(metal::MTLBlendFactor::One);
    color_attachment.set_destination_rgb_blend_factor(metal::MTLBlendFactor::OneMinusSourceAlpha);
    color_attachment.set_destination_alpha_blend_factor(metal::MTLBlendFactor::OneMinusSourceAlpha);

    device
        .new_render_pipeline_state(&descriptor)
        .expect("could not create render pipeline state")
}

/// Pipeline that writes its fragment output verbatim — no blending.
///
/// The Kawase passes render a filtered *copy* into a scratch texture; blending
/// them onto whatever the scratch texture held last frame would smear frames
/// together.
/// Tap offsets, in half-resolution texels, for a Kawase chain approximating a
/// Gaussian of `radius_half_res`.
///
/// Each pass adds roughly its own offset to the total reach, so a handful of
/// growing offsets covers a wide radius at a fraction of what a separable
/// Gaussian of the same width would cost. Capped at five passes: past that the
/// chain costs more than it visibly improves.
/// The region the blur chain has to cover, normalised to the target: the union
/// of the batch's rects, grown by the blur's reach and clamped to the viewport.
///
/// The chain used to run over the whole viewport, which made its cost a
/// function of the window size rather than of how much was actually blurred —
/// measured 2026-07-29 at +0.99 ms per frame on an 1800x1200 window for a
/// dialog covering 2.6% of it.
///
/// The growth matters: without it the chain samples texels just outside each
/// rect that no pass has written, and the surface picks up a dark rim. Shading
/// a few extra rows is far cheaper than that artefact, so the margin is
/// deliberately generous.
///
/// `None` when the union is empty, which skips the chain entirely.
fn blur_region(
    blurs: &[BackdropBlur],
    blur_radius: f32,
    width: u64,
    height: u64,
) -> Option<(f32, f32, f32, f32)> {
    let (mut left, mut top) = (f32::MAX, f32::MAX);
    let (mut right, mut bottom) = (f32::MIN, f32::MIN);
    for blur in blurs {
        let bounds = blur.bounds;
        left = left.min(bounds.origin.x.0);
        top = top.min(bounds.origin.y.0);
        right = right.max(bounds.origin.x.0 + bounds.size.width.0);
        bottom = bottom.max(bounds.origin.y.0 + bounds.size.height.0);
    }
    if !(left < right && top < bottom) {
        return None;
    }

    let margin = blur_radius * 1.5 + 4.0;
    let (width, height) = (width as f32, height as f32);
    let left = (left - margin).max(0.0);
    let top = (top - margin).max(0.0);
    let right = (right + margin).min(width);
    let bottom = (bottom + margin).min(height);
    if !(left < right && top < bottom) {
        return None;
    }
    Some((
        left / width,
        top / height,
        (right - left) / width,
        (bottom - top) / height,
    ))
}

fn kawase_offsets(radius_half_res: f32) -> Vec<f32> {
    if !(radius_half_res > 0.5) {
        return Vec::new();
    }
    let count = (radius_half_res.sqrt().round() as usize).clamp(1, 5);
    // Offsets (i + 0.5) * scale sum to `radius_half_res`.
    let scale = 2.0 * radius_half_res / (count * count) as f32;
    (0..count).map(|i| (i as f32 + 0.5) * scale).collect()
}

fn build_blit_pipeline_state(
    device: &metal::DeviceRef,
    library: &metal::LibraryRef,
    label: &str,
    vertex_fn_name: &str,
    fragment_fn_name: &str,
    pixel_format: metal::MTLPixelFormat,
) -> metal::RenderPipelineState {
    let vertex_fn = library
        .get_function(vertex_fn_name, None)
        .expect("error locating vertex function");
    let fragment_fn = library
        .get_function(fragment_fn_name, None)
        .expect("error locating fragment function");

    let descriptor = metal::RenderPipelineDescriptor::new();
    descriptor.set_label(label);
    descriptor.set_vertex_function(Some(vertex_fn.as_ref()));
    descriptor.set_fragment_function(Some(fragment_fn.as_ref()));
    let color_attachment = descriptor.color_attachments().object_at(0).unwrap();
    color_attachment.set_pixel_format(pixel_format);
    color_attachment.set_blending_enabled(false);

    device
        .new_render_pipeline_state(&descriptor)
        .expect("could not create render pipeline state")
}

fn build_path_sprite_pipeline_state(
    device: &metal::DeviceRef,
    library: &metal::LibraryRef,
    label: &str,
    vertex_fn_name: &str,
    fragment_fn_name: &str,
    pixel_format: metal::MTLPixelFormat,
) -> metal::RenderPipelineState {
    let vertex_fn = library
        .get_function(vertex_fn_name, None)
        .expect("error locating vertex function");
    let fragment_fn = library
        .get_function(fragment_fn_name, None)
        .expect("error locating fragment function");

    let descriptor = metal::RenderPipelineDescriptor::new();
    descriptor.set_label(label);
    descriptor.set_vertex_function(Some(vertex_fn.as_ref()));
    descriptor.set_fragment_function(Some(fragment_fn.as_ref()));
    let color_attachment = descriptor.color_attachments().object_at(0).unwrap();
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
        .expect("could not create render pipeline state")
}

fn build_path_rasterization_pipeline_state(
    device: &metal::DeviceRef,
    library: &metal::LibraryRef,
    label: &str,
    vertex_fn_name: &str,
    fragment_fn_name: &str,
    pixel_format: metal::MTLPixelFormat,
    path_sample_count: u32,
) -> metal::RenderPipelineState {
    let vertex_fn = library
        .get_function(vertex_fn_name, None)
        .expect("error locating vertex function");
    let fragment_fn = library
        .get_function(fragment_fn_name, None)
        .expect("error locating fragment function");

    let descriptor = metal::RenderPipelineDescriptor::new();
    descriptor.set_label(label);
    descriptor.set_vertex_function(Some(vertex_fn.as_ref()));
    descriptor.set_fragment_function(Some(fragment_fn.as_ref()));
    if path_sample_count > 1 {
        descriptor.set_raster_sample_count(path_sample_count as _);
        descriptor.set_alpha_to_coverage_enabled(false);
    }
    let color_attachment = descriptor.color_attachments().object_at(0).unwrap();
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
        .expect("could not create render pipeline state")
}

#[derive(Clone)]
struct InstanceBinding {
    buffer: metal::Buffer,
    offset: usize,
}

struct InstanceBindings {
    quads: InstanceBinding,
    shadows: InstanceBinding,
    underlines: InstanceBinding,
    monochrome_sprites: InstanceBinding,
    polychrome_sprites: InstanceBinding,
    surfaces: InstanceBinding,
    backdrop_blurs: InstanceBinding,
    custom_shaders: InstanceBinding,
}

fn write_instances(scene: &Scene, writer: &mut InstanceBufferWriter) -> Result<InstanceBindings> {
    Ok(InstanceBindings {
        quads: writer.write(&scene.quads)?,
        shadows: writer.write(&scene.shadows)?,
        underlines: writer.write(&scene.underlines)?,
        monochrome_sprites: writer.write(&scene.monochrome_sprites)?,
        polychrome_sprites: writer.write(&scene.polychrome_sprites)?,
        surfaces: writer.write_iter(scene.surfaces.iter().map(|surface| SurfaceBounds {
            bounds: surface.bounds,
            content_mask: surface.content_mask,
        }))?,
        backdrop_blurs: writer.write(&scene.backdrop_blurs)?,
        custom_shaders: writer.write(&scene.custom_shaders)?,
    })
}

struct InstanceBufferWriter {
    device: metal::Device,
    pool: Arc<Mutex<InstanceBufferPool>>,
    unified_memory: bool,
    filled: Vec<(InstanceBuffer, usize)>,
    current: InstanceBuffer,
    offset: usize,
}

impl InstanceBufferWriter {
    fn new(
        device: &metal::Device,
        pool: &Arc<Mutex<InstanceBufferPool>>,
        unified_memory: bool,
    ) -> Self {
        let current = pool.lock().acquire(device, unified_memory);
        Self {
            device: device.clone(),
            pool: pool.clone(),
            unified_memory,
            filled: Vec::new(),
            current,
            offset: 0,
        }
    }

    fn allocate<T>(&mut self, count: usize) -> Result<(InstanceBinding, &mut [MaybeUninit<T>])> {
        let size = mem::size_of::<T>() * count;
        let mut offset = self.offset.next_multiple_of(INSTANCE_BUFFER_ALIGNMENT);
        if offset + size > self.current.size {
            self.grow(size)?;
            offset = 0;
        }
        self.offset = offset + size;

        let binding = InstanceBinding {
            buffer: self.current.metal_buffer.clone(),
            offset,
        };
        // Safety: the reservation lies within a buffer this frame owns
        // exclusively, and never overlaps one handed out earlier.
        let values = unsafe {
            let start = (self.current.metal_buffer.contents() as *mut u8).add(offset);
            slice::from_raw_parts_mut(start.cast::<MaybeUninit<T>>(), count)
        };
        Ok((binding, values))
    }

    fn write<T>(&mut self, values: &[T]) -> Result<InstanceBinding> {
        let (binding, destination) = self.allocate::<T>(values.len())?;
        unsafe {
            ptr::copy_nonoverlapping(
                values.as_ptr(),
                destination.as_mut_ptr().cast::<T>(),
                values.len(),
            );
        }
        Ok(binding)
    }

    fn write_iter<T>(
        &mut self,
        values: impl ExactSizeIterator<Item = T>,
    ) -> Result<InstanceBinding> {
        let (binding, destination) = self.allocate::<T>(values.len())?;
        for (slot, value) in destination.iter_mut().zip(values) {
            slot.write(value);
        }
        Ok(binding)
    }

    fn grow(&mut self, required: usize) -> Result<()> {
        let mut pool = self.pool.lock();
        let buffer_size = (pool.buffer_size * 2)
            .max(required.next_power_of_two())
            .min(MAX_INSTANCE_BUFFER_SIZE);
        anyhow::ensure!(
            buffer_size >= required,
            "instance buffer needs {required} bytes, above the maximum of {MAX_INSTANCE_BUFFER_SIZE}"
        );
        anyhow::ensure!(
            buffer_size > self.current.size,
            "frame instance data exceeds the {MAX_INSTANCE_BUFFER_SIZE}-byte maximum"
        );
        if buffer_size != pool.buffer_size {
            log::info!("increased instance buffer size to {buffer_size}");
            pool.reset(buffer_size);
        }
        let buffer = pool.acquire(&self.device, self.unified_memory);
        drop(pool);

        let filled = mem::replace(&mut self.current, buffer);
        self.filled.push((filled, self.offset));
        self.offset = 0;
        Ok(())
    }

    fn finish(self) -> InstanceBuffer {
        let Self {
            unified_memory,
            filled,
            current,
            offset,
            ..
        } = self;

        if !unified_memory {
            for (buffer, written) in &filled {
                if *written == 0 {
                    continue;
                }
                buffer.metal_buffer.did_modify_range(NSRange {
                    location: 0,
                    length: *written as NSUInteger,
                });
            }
            if offset > 0 {
                current.metal_buffer.did_modify_range(NSRange {
                    location: 0,
                    length: offset as NSUInteger,
                });
            }
        }

        // Metal retains encoded resources until the command buffer completes.
        // Only the final, largest buffer is worth keeping in the pool.
        drop(filled);
        current
    }
}

#[repr(C)]
enum ShadowInputIndex {
    Vertices = 0,
    Shadows = 1,
    ViewportSize = 2,
}

#[repr(C)]
enum QuadInputIndex {
    Vertices = 0,
    Quads = 1,
    ViewportSize = 2,
}

#[repr(C)]
enum BackdropBlurInputIndex {
    Vertices = 0,
    BackdropBlurs = 1,
    ViewportSize = 2,
    BlurredTexture = 3,
}

#[repr(C)]
enum KawaseInputIndex {
    Vertices = 0,
    Params = 1,
    SourceTexture = 2,
}

/// Uniforms for one dual-Kawase pass. Mirrored in `shaders.metal`.
///
/// All fields are plain arrays rather than vectors: cbindgen renders them as C
/// arrays, and Metal will not apply vector alignment to those.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct KawaseParams {
    /// 1 / source texture size, so `offset` can be expressed in texels.
    pub texel: [f32; 2],
    /// Tap offset in source texels.
    pub offset: f32,
    pub _pad: f32,
    /// Top-left of the region to shade, normalised to the target.
    ///
    /// The chain runs over the union of the frame's blur rects, not the whole
    /// viewport: the cost is otherwise set by the window size rather than by
    /// how much is actually being blurred — measured 2026-07-29 at +0.99 ms per
    /// frame on an 1800x1200 window for a dialog covering 2.6% of it.
    pub origin: [f32; 2],
    /// Extent of that region, normalised to the target.
    pub size: [f32; 2],
}

#[repr(C)]
enum UnderlineInputIndex {
    Vertices = 0,
    Underlines = 1,
    ViewportSize = 2,
}

#[repr(C)]
enum SpriteInputIndex {
    Vertices = 0,
    Sprites = 1,
    ViewportSize = 2,
    AtlasTextureSize = 3,
    AtlasTexture = 4,
}

#[repr(C)]
enum SurfaceInputIndex {
    Vertices = 0,
    Surfaces = 1,
    ViewportSize = 2,
    TextureSize = 3,
    YTexture = 4,
    CbCrTexture = 5,
}

#[repr(C)]
enum PathRasterizationInputIndex {
    Vertices = 0,
    ViewportSize = 1,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct PathSprite {
    pub bounds: Bounds<ScaledPixels>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct SurfaceBounds {
    pub bounds: Bounds<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
}

#[cfg(any(test, feature = "bench-support", feature = "test-support"))]
pub struct MetalHeadlessRenderer {
    renderer: MetalRenderer,
}

#[cfg(any(test, feature = "bench-support", feature = "test-support"))]
impl MetalHeadlessRenderer {
    pub fn new() -> Self {
        let instance_buffer_pool = Arc::new(Mutex::new(InstanceBufferPool::default()));
        let renderer = MetalRenderer::new_headless(instance_buffer_pool);
        Self { renderer }
    }
}

#[cfg(any(test, feature = "bench-support", feature = "test-support"))]
impl gpui::PlatformHeadlessRenderer for MetalHeadlessRenderer {
    fn render_scene_to_image(
        &mut self,
        scene: &Scene,
        size: Size<DevicePixels>,
    ) -> anyhow::Result<image::RgbaImage> {
        self.renderer.render_scene_to_image(scene, size)
    }

    fn render_scene(&mut self, scene: &Scene, size: Size<DevicePixels>) -> anyhow::Result<()> {
        self.renderer.render_scene(scene, size)
    }

    fn sprite_atlas(&self) -> Arc<dyn gpui::PlatformAtlas> {
        self.renderer.sprite_atlas().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_core_foundation::{CFDictionary, CFType};
    use objc2_core_video::{
        CVPixelBufferCreate, CVPixelBufferGetBaseAddressOfPlane,
        CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
        CVPixelBufferUnlockBaseAddress, kCVPixelBufferIOSurfacePropertiesKey,
    };

    #[test]
    fn render_core_video_surface() {
        let attributes = CFDictionary::from_slices(
            &[unsafe { kCVPixelBufferIOSurfacePropertiesKey }],
            &[&*CFDictionary::<CFType, CFType>::empty()],
        );
        let mut image_buffer = ptr::null_mut();
        assert_eq!(
            unsafe {
                CVPixelBufferCreate(
                    None,
                    16,
                    16,
                    kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
                    Some(attributes.as_opaque()),
                    ptr::NonNull::from(&mut image_buffer),
                )
            },
            kCVReturnSuccess
        );
        let image_buffer =
            unsafe { CFRetained::from_raw(ptr::NonNull::new(image_buffer).unwrap()) };
        unsafe {
            assert_eq!(
                CVPixelBufferLockBaseAddress(&image_buffer, CVPixelBufferLockFlags::empty()),
                kCVReturnSuccess
            );
            for plane in 0..2 {
                let bytes = CVPixelBufferGetBaseAddressOfPlane(&image_buffer, plane).cast::<u8>();
                let length = CVPixelBufferGetBytesPerRowOfPlane(&image_buffer, plane)
                    * CVPixelBufferGetHeightOfPlane(&image_buffer, plane);
                slice::from_raw_parts_mut(bytes, length).fill(128);
            }
            assert_eq!(
                CVPixelBufferUnlockBaseAddress(&image_buffer, CVPixelBufferLockFlags::empty()),
                kCVReturnSuccess
            );
        }

        let bounds = Bounds {
            origin: Point::default(),
            size: size(ScaledPixels(16.0), ScaledPixels(16.0)),
        };
        let mut scene = Scene::default();
        scene.insert_primitive(PaintSurface {
            order: 0,
            bounds,
            content_mask: ContentMask { bounds },
            image_buffer: image_buffer.clone(),
        });
        drop(image_buffer);
        scene.finish();

        let mut renderer = MetalRenderer::new_headless(Arc::new(Mutex::new(Default::default())));
        let image = renderer
            .render_scene_to_image(&scene, size(DevicePixels(16), DevicePixels(16)))
            .unwrap();
        for image::Rgba([red, green, blue, alpha]) in image.pixels() {
            assert!(red.abs_diff(128) <= 2);
            assert!(green.abs_diff(128) <= 2);
            assert!(blue.abs_diff(128) <= 2);
            assert_eq!(*alpha, 255);
        }
    }
}
