//! Shared-D3D11-texture video frames for zero-copy presentation on Windows.
//!
//! A decoder (e.g. Media Foundation's media engine) renders its decoded,
//! colour-converted frame into a BGRA texture created with
//! `D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX | D3D11_RESOURCE_MISC_SHARED_NTHANDLE`
//! on its own device and exports it via `IDXGIResource1::CreateSharedHandle`;
//! the DirectX renderer opens the same allocation with
//! `ID3D11Device1::OpenSharedResource1` and samples it directly — no
//! readback, no copy. See `gpui::surface()` for the element that presents
//! one, and `DmabufFrame` for the Linux counterpart.

use std::sync::Arc;

/// A shared-texture video frame (BGRA, keyed-mutex synchronized).
///
/// Plain data plus an opaque `owner` that keeps the producer's texture (and
/// with it the shared handle) alive for as long as any clone of the frame —
/// or any GPU work sampling it — exists.
#[derive(Clone)]
pub struct D3d11Frame {
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// NT shared handle from `IDXGIResource1::CreateSharedHandle`. Borrowed —
    /// kept open by `owner`; the renderer opens (and caches) its own view of
    /// the texture from it.
    pub shared_handle: isize,
    /// Keyed-mutex key the consumer acquires before sampling (the producer
    /// releases to this key after writing). `u64::MAX` = no keyed mutex.
    pub acquire_key: u64,
    /// Key the consumer releases to when done, handing the texture back to
    /// the producer.
    pub release_key: u64,
    /// Keeps the producer's texture alive; dropped when the last clone of
    /// the frame (including the renderer's cached import) is released.
    pub owner: Arc<dyn std::any::Any + Send + Sync>,
}

impl std::fmt::Debug for D3d11Frame {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("D3d11Frame")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("shared_handle", &format_args!("{:#x}", self.shared_handle))
            .field("acquire_key", &self.acquire_key)
            .field("release_key", &self.release_key)
            .finish_non_exhaustive()
    }
}

impl PartialEq for D3d11Frame {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.owner, &other.owner)
            && self.shared_handle == other.shared_handle
            && self.width == other.width
            && self.height == other.height
            && self.acquire_key == other.acquire_key
            && self.release_key == other.release_key
    }
}

impl Eq for D3d11Frame {}
