//! `open_with_system` through `UIActivityViewController`.
//!
//! iOS has no "default app" for a file: handing one to another app is the
//! share sheet, whose app row is "Open in …", next to AirDrop and "Save to
//! Files". On an iPad the sheet is a popover and must have an anchor, or
//! UIKit raises; it points at where the pointer or the last touch was, which
//! is where the user asked for it, and sits in the middle without an arrow
//! while there was none.

use std::path::Path;

use gpui::Point;
use objc2::{MainThreadOnly, rc::Retained};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::{NSArray, NSString, NSURL};
use objc2_ui_kit::{UIActivityViewController, UIPopoverArrowDirection, UIViewController};

use super::window::pointer_position_of;

/// Presents the share sheet for `path` on `presenter`; `root` is the window's
/// own view controller, which knows where the pointer is.
pub(super) fn present(root: &UIViewController, presenter: &UIViewController, path: &Path) {
    // A sheet already open stays the only one; a second would stack on it.
    if presenter
        .downcast_ref::<UIActivityViewController>()
        .is_some()
    {
        log::debug!("GPUI iOS: a share sheet is already open");
        return;
    }
    let Some(path_string) = path.to_str() else {
        log::error!("GPUI iOS: cannot share a path that is not UTF-8: {path:?}");
        return;
    };
    let url = NSURL::fileURLWithPath(&NSString::from_str(path_string));
    let items: Retained<NSArray> =
        NSArray::from_retained_slice(&[Retained::into_super(Retained::into_super(url))]);
    // SAFETY: the activity items are NSURLs, which the sheet accepts.
    let sheet = unsafe {
        UIActivityViewController::initWithActivityItems_applicationActivities(
            UIActivityViewController::alloc(presenter.mtm()),
            &items,
            None,
        )
    };
    if let Some(popover) = sheet.popoverPresentationController() {
        // Only the root's view shares coordinates with the pointer; over
        // something presented, the presenter's middle stands in.
        let anchor_view = if std::ptr::eq(root, presenter) {
            root.view()
        } else {
            presenter.view()
        };
        let Some(anchor_view) = anchor_view else {
            log::error!("GPUI iOS: no view to anchor the share sheet on");
            return;
        };
        // The origin is what a window reports before any pointer or touch
        // arrived; an arrow into the corner would point at nothing.
        let pointer = pointer_position_of(root)
            .filter(|position| std::ptr::eq(root, presenter) && *position != Point::default());
        let point = match pointer {
            Some(position) => {
                CGPoint::new(f32::from(position.x).into(), f32::from(position.y).into())
            }
            None => {
                popover.setPermittedArrowDirections(UIPopoverArrowDirection::empty());
                let bounds = anchor_view.bounds();
                CGPoint::new(
                    bounds.origin.x + bounds.size.width / 2.0,
                    bounds.origin.y + bounds.size.height / 2.0,
                )
            }
        };
        popover.setSourceView(Some(&anchor_view));
        popover.setSourceRect(CGRect::new(point, CGSize::new(1.0, 1.0)));
    }
    presenter.presentViewController_animated_completion(&sheet, true, None);
}
