use crate::{
    App, Bounds, Element, ElementId, GlobalElementId, InspectorElementId, IntoElement, LayoutId,
    ObjectFit, Pixels, Style, StyleRefinement, Styled, Window,
};
#[cfg(target_os = "macos")]
use core_video::pixel_buffer::CVPixelBuffer;
use refineable::Refineable;

/// A source of a surface's content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SurfaceSource {
    /// A macOS image buffer from CoreVideo
    #[cfg(target_os = "macos")]
    Surface(CVPixelBuffer),
    /// A DMABuf-backed video frame, imported zero-copy by the Vulkan
    /// renderer. Check [`Window::supports_dmabuf_surfaces`] before using;
    /// unsupported frames render as nothing.
    #[cfg(target_os = "linux")]
    Dmabuf(crate::DmabufFrame),
    /// A shared-D3D11-texture video frame, imported zero-copy by the DirectX
    /// renderer. Check [`Window::supports_video_surfaces`] before using;
    /// unsupported frames render as nothing.
    #[cfg(target_os = "windows")]
    D3d11(crate::D3d11Frame),
}

#[cfg(target_os = "macos")]
impl From<CVPixelBuffer> for SurfaceSource {
    fn from(value: CVPixelBuffer) -> Self {
        SurfaceSource::Surface(value)
    }
}

#[cfg(target_os = "linux")]
impl From<crate::DmabufFrame> for SurfaceSource {
    fn from(value: crate::DmabufFrame) -> Self {
        SurfaceSource::Dmabuf(value)
    }
}

#[cfg(target_os = "windows")]
impl From<crate::D3d11Frame> for SurfaceSource {
    fn from(value: crate::D3d11Frame) -> Self {
        SurfaceSource::D3d11(value)
    }
}

/// A surface element.
pub struct Surface {
    source: SurfaceSource,
    object_fit: ObjectFit,
    style: StyleRefinement,
}

/// Create a new surface element.
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
pub fn surface(source: impl Into<SurfaceSource>) -> Surface {
    Surface {
        source: source.into(),
        object_fit: ObjectFit::Contain,
        style: Default::default(),
    }
}

impl Surface {
    /// Set the object fit for the image.
    pub fn object_fit(mut self, object_fit: ObjectFit) -> Self {
        self.object_fit = object_fit;
        self
    }
}

impl Element for Surface {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.refine(&self.style);
        let layout_id = window.request_layout(style, [], cx);
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
    }

    fn paint(
        &mut self,
        _global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        #[cfg_attr(
            not(any(target_os = "macos", target_os = "linux", target_os = "windows")),
            allow(unused_variables)
        )]
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        _: &mut Self::PrepaintState,
        #[cfg_attr(
            not(any(target_os = "macos", target_os = "linux", target_os = "windows")),
            allow(unused_variables)
        )]
        window: &mut Window,
        _: &mut App,
    ) {
        match &self.source {
            #[cfg(target_os = "macos")]
            SurfaceSource::Surface(surface) => {
                let size = crate::size(surface.get_width().into(), surface.get_height().into());
                let new_bounds = self.object_fit.get_bounds(bounds, size);
                // TODO: Add support for corner_radii
                window.paint_surface(new_bounds, surface.clone());
            }
            #[cfg(target_os = "linux")]
            SurfaceSource::Dmabuf(frame) => {
                let size = crate::size(
                    crate::DevicePixels(frame.width as i32),
                    crate::DevicePixels(frame.height as i32),
                );
                let new_bounds = self.object_fit.get_bounds(bounds, size);
                window.paint_surface(new_bounds, frame.clone());
            }
            #[cfg(target_os = "windows")]
            SurfaceSource::D3d11(frame) => {
                let size = crate::size(
                    crate::DevicePixels(frame.width as i32),
                    crate::DevicePixels(frame.height as i32),
                );
                let new_bounds = self.object_fit.get_bounds(bounds, size);
                window.paint_surface(new_bounds, frame.clone());
            }
            #[allow(unreachable_patterns)]
            _ => {}
        }
    }
}

impl IntoElement for Surface {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Styled for Surface {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}
