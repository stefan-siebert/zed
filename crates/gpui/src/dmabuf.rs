//! DMABuf-backed video frames for zero-copy presentation on Linux.
//!
//! A decoder (e.g. GStreamer with VA-API) exports its decoded surface as a
//! DMABuf and describes the memory layout here; the wgpu renderer imports it
//! as a Vulkan image (`VK_EXT_image_drm_format_modifier` +
//! `VK_EXT_external_memory_dma_buf`) and samples it directly — no readback,
//! no copy. See `gpui::surface()` for the element that presents one.

use std::sync::Arc;

/// DRM fourcc for two-plane Y/CbCr 4:2:0 (`NV12`), the format hardware
/// decoders produce. The only format the renderer currently imports.
pub const DRM_FOURCC_NV12: u32 = u32::from_le_bytes(*b"NV12");

/// A DRM pixel format + tiling modifier pair a renderer can import. Video
/// producers negotiate their export format against this list (the same
/// contract Wayland compositors use).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DmabufFormat {
    /// DRM fourcc (e.g. [`DRM_FOURCC_NV12`]).
    pub fourcc: u32,
    /// DRM format modifier (tiling); `0` is linear.
    pub modifier: u64,
}

/// One memory plane of a [`DmabufFrame`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DmabufPlane {
    /// The dmabuf file descriptor backing this plane. Borrowed — kept open by
    /// [`DmabufFrame::owner`]; importers must `dup()` it if the import API
    /// takes ownership (Vulkan's `VK_KHR_external_memory_fd` does).
    pub fd: std::os::fd::RawFd,
    /// Byte offset of this plane within its dmabuf.
    pub offset: u32,
    /// Row pitch in bytes.
    pub stride: u32,
}

/// A DMABuf-backed video frame.
///
/// Plain data plus an opaque `owner` that keeps the producer's buffer (and
/// with it the file descriptors) alive for as long as any clone of the frame
/// — or any GPU work sampling it — exists.
#[derive(Clone)]
pub struct DmabufFrame {
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// DRM fourcc of the pixel format (currently only [`DRM_FOURCC_NV12`]).
    pub fourcc: u32,
    /// DRM format modifier describing the memory tiling layout.
    pub modifier: u64,
    /// Memory planes (NV12: luma then interleaved chroma; the planes may
    /// share a single fd with different offsets).
    pub planes: Vec<DmabufPlane>,
    /// Column-major YCbCr→RGB matrix matching the frame's colorimetry
    /// (applied to the (Y, Cb, Cr, 1) vector). Producers derive it from the
    /// stream (e.g. BT.709 limited-range for typical H.264);
    /// [`ycbcr_bt601_full`] is a reasonable default.
    pub color_matrix: [[f32; 4]; 4],
    /// Keeps the producer's buffer alive; dropped when the last clone of the
    /// frame (including the renderer's imported texture) is released.
    pub owner: Arc<dyn std::any::Any + Send + Sync>,
}

/// YCbCr→RGB for BT.601 full range (JPEG-style; the historical default of
/// gpui's surface shader).
pub fn ycbcr_bt601_full() -> [[f32; 4]; 4] {
    // Column-major: columns multiply Y, Cb, Cr, 1 respectively.
    [
        [1.0000, 1.0000, 1.0000, 0.0],
        [0.0000, -0.3441, 1.7720, 0.0],
        [1.4020, -0.7141, 0.0000, 0.0],
        [-0.7010, 0.5291, -0.8860, 1.0],
    ]
}

/// YCbCr→RGB for BT.601 limited (video) range.
pub fn ycbcr_bt601_limited() -> [[f32; 4]; 4] {
    [
        [1.16438, 1.16438, 1.16438, 0.0],
        [0.00000, -0.39176, 2.01723, 0.0],
        [1.59603, -0.81297, 0.00000, 0.0],
        [-0.87108, 0.52930, -1.08167, 1.0],
    ]
}

/// YCbCr→RGB for BT.709 limited (video) range — what hardware decoders
/// produce for typical HD H.264/H.265 content.
pub fn ycbcr_bt709_limited() -> [[f32; 4]; 4] {
    [
        [1.16438, 1.16438, 1.16438, 0.0],
        [0.00000, -0.21325, 2.11240, 0.0],
        [1.79274, -0.53291, 0.00000, 0.0],
        [-0.96943, 0.30002, -1.12926, 1.0],
    ]
}

/// YCbCr→RGB for BT.2020 (non-constant luminance) limited range.
pub fn ycbcr_bt2020_limited() -> [[f32; 4]; 4] {
    [
        [1.16438, 1.16438, 1.16438, 0.0],
        [0.00000, -0.18734, 2.14177, 0.0],
        [1.67867, -0.65042, 0.00000, 0.0],
        [-0.91240, 0.34583, -1.14807, 1.0],
    ]
}

impl std::fmt::Debug for DmabufFrame {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DmabufFrame")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("fourcc", &self.fourcc)
            .field("modifier", &format_args!("{:#018x}", self.modifier))
            .field("planes", &self.planes)
            .finish_non_exhaustive()
    }
}

impl PartialEq for DmabufFrame {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.owner, &other.owner)
            && self.planes == other.planes
            && self.width == other.width
            && self.height == other.height
            && self.fourcc == other.fourcc
            && self.modifier == other.modifier
            && self.color_matrix == other.color_matrix
    }
}

impl Eq for DmabufFrame {}
