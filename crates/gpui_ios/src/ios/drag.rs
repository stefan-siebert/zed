//! Dragging files out of a GPUI app, as system drags.
//!
//! UIKit starts a drag only from a `UIDragInteraction`, by its own gesture:
//! when it recognizes one it asks which items to lift. So the window asks
//! GPUI then (`PlatformWindow::on_platform_drag_request`): a drag GPUI is
//! already running is handed over, or the element under the touch with an
//! external drag payload starts its drag. The drag is the platform's from
//! then on; over the window it came from GPUI gets it back (`drop.rs`), and
//! a drop there is GPUI's own drop, not a file drop.
//!
//! A pointer drag waits for the lift delay like a touch does
//! (`allowsPointerDragBeforeLiftDelay`, iPadOS 27+): a quick drag stays GPUI's
//! drag inside the window, holding first lifts it out to other apps. Before
//! iPadOS 27 a pointer drag lifts at once.
//!
//! The lift uses the extended delay (`liftBehavior`, iPadOS 27+). GPUI's long
//! press is a timer on the raw touches, not a recognizer, and the default
//! lift cancelled those touches before it ran: a long press lifted the drag
//! and never opened the context menu. With the extended delay the long press
//! comes first, then the lift; let go without moving and the lift ends where
//! it began, which UIKit reports only through the lift animation's completion
//! (`lift_animating`), not as the end of a drag. Measured in the iPadOS 27.0
//! simulator: a finger held 0.8 s opens the menu and lifts, a pointer held
//! 0.6 s still drags inside GPUI, 1 s lifts.
//!
//! Each file goes out as a file representation for opening in place, so the
//! app it is dropped on gets the original, not a copy.

use std::{path::PathBuf, rc::Weak};

use block2::{DynBlock, RcBlock};
use gpui::{ExternalDragPayload, FileDropEvent, PlatformInput, Point, px};
use objc2::{
    MainThreadMarker, MainThreadOnly,
    rc::Retained,
    runtime::{Bool, ProtocolObject},
    sel,
};
use objc2_core_foundation::CGPoint;
use objc2_foundation::{
    NSArray, NSError, NSItemProvider, NSItemProviderFileOptions,
    NSItemProviderRepresentationVisibility, NSObjectProtocol, NSProgress, NSString, NSURL,
};
use objc2_ui_kit::{
    UIColor, UIDragAnimating, UIDragDropSession, UIDragInteraction, UIDragInteractionDelegate,
    UIDragItem, UIDragPreviewParameters, UIDragPreviewTarget, UIDragSession, UIFont, UIInteraction,
    UILabel, UITargetedDragPreview, UIView, UIViewAnimatingPosition,
};
use objc2_uniform_type_identifiers::UTType;

use super::window::IosWindowState;

/// Padding around the name in the lift preview, in points.
const PREVIEW_PADDING: f64 = 10.;

/// Adds the drag interaction to `view`, which keeps it.
pub(super) fn install(
    view: &UIView,
    delegate: &ProtocolObject<dyn UIDragInteractionDelegate>,
    main_thread: MainThreadMarker,
) {
    let interaction =
        UIDragInteraction::initWithDelegate(UIDragInteraction::alloc(main_thread), delegate);
    // On by default only on iPad.
    interaction.setEnabled(true);
    let wait_for_lift = sel!(setAllowsPointerDragBeforeLiftDelay:);
    if interaction.respondsToSelector(wait_for_lift) {
        // SAFETY: the setter takes a BOOL, and the interaction responds to it.
        unsafe {
            let _: () = objc2::msg_send![&interaction, setAllowsPointerDragBeforeLiftDelay: false];
        }
    }
    if interaction.respondsToSelector(sel!(setLiftBehavior:)) {
        // SAFETY: the setter takes a `UIDragLiftBehavior` (NSUInteger), and
        // the interaction responds to it; 1 is `UIDragLiftBehaviorExtended`.
        unsafe {
            let _: () = objc2::msg_send![&interaction, setLiftBehavior: 1usize];
        }
    }
    view.addInteraction(ProtocolObject::<dyn UIInteraction>::from_ref(&*interaction));
}

/// The items to lift from where `session` began, or none to let the gesture
/// fail.
pub(super) fn items(
    window: &IosWindowState,
    session: &ProtocolObject<dyn UIDragSession>,
    view: &UIView,
) -> Retained<NSArray<UIDragItem>> {
    let location = session.locationInView(view);
    let position = Point::new(px(location.x as f32), px(location.y as f32));
    let items: Vec<_> = match window.request_platform_drag(position) {
        Some(ExternalDragPayload::Files(paths)) => paths
            .entries()
            .iter()
            .map(|(path, is_dir)| item(path.clone(), *is_dir, view.mtm()))
            .collect(),
        None => Vec::new(),
    };
    window.set_drag_source(!items.is_empty());
    NSArray::from_retained_slice(&items)
}

/// The lift is animating: a lift that ends where it started is a drag that
/// never began, which UIKit reports nowhere else.
pub(super) fn lift_animating(
    window: Weak<IosWindowState>,
    animator: &ProtocolObject<dyn UIDragAnimating>,
) {
    let completion = RcBlock::new(move |position: UIViewAnimatingPosition| {
        if position == UIViewAnimatingPosition::Start
            && let Some(window) = window.upgrade()
        {
            ended(&window);
        }
    });
    animator.addCompletion(&completion);
}

/// The drag ended, dropped or not.
pub(super) fn ended(window: &IosWindowState) {
    window.set_drag_source(false);
    window.dispatch_input(PlatformInput::FileDrop(FileDropEvent::Ended));
}

fn item(path: PathBuf, is_dir: bool, main_thread: MainThreadMarker) -> Retained<UIDragItem> {
    let provider = NSItemProvider::new();
    let type_identifier = type_identifier(&path, is_dir);
    let name = path
        .file_name()
        .map(|name| NSString::from_str(&name.to_string_lossy()));
    provider.setSuggestedName(name.as_deref());
    let load = RcBlock::new(
        move |completion: std::ptr::NonNull<DynBlock<dyn Fn(*mut NSURL, Bool, *mut NSError)>>| {
            let url = NSURL::from_file_path(&path);
            // SAFETY: UIKit passes a valid completion handler for the call.
            let completion = unsafe { completion.as_ref() };
            match url {
                Some(url) => completion.call((
                    Retained::as_ptr(&url).cast_mut(),
                    Bool::NO,
                    std::ptr::null_mut(),
                )),
                None => {
                    log::warn!("GPUI iOS: no file URL for {}", path.display());
                    completion.call((std::ptr::null_mut(), Bool::NO, std::ptr::null_mut()))
                }
            }
            std::ptr::null_mut::<NSProgress>()
        },
    );
    // SAFETY: the handler matches the declared block type and calls the
    // completion handler once.
    unsafe {
        provider.registerFileRepresentationForTypeIdentifier_fileOptions_visibility_loadHandler(
            &type_identifier,
            NSItemProviderFileOptions::OpenInPlace,
            NSItemProviderRepresentationVisibility::All,
            &load,
        );
    }
    UIDragItem::initWithItemProvider(UIDragItem::alloc(main_thread), &provider)
}

fn type_identifier(path: &std::path::Path, is_dir: bool) -> Retained<NSString> {
    if is_dir {
        return NSString::from_str("public.folder");
    }
    path.extension()
        .and_then(|extension| {
            UTType::typeWithFilenameExtension(&NSString::from_str(&extension.to_string_lossy()))
        })
        .map(|uniform_type| uniform_type.identifier())
        .unwrap_or_else(|| NSString::from_str("public.data"))
}

/// The lift preview of `item`: its name on a rounded plate where the touch
/// began. GPUI draws everything into one Metal layer, so there is no view of
/// the element to lift.
pub(super) fn lift_preview(
    item: &UIDragItem,
    session: &ProtocolObject<dyn UIDragSession>,
    view: &UIView,
) -> Retained<UITargetedDragPreview> {
    let main_thread = view.mtm();
    let name = item
        .itemProvider()
        .suggestedName()
        .unwrap_or_else(|| NSString::from_str("…"));
    let label = UILabel::new(main_thread);
    label.setText(Some(&name));
    // SAFETY: an extern constant UIKit defines; font and colour are only
    // unsafe to set because the setters take any object.
    unsafe {
        label.setFont(Some(&UIFont::preferredFontForTextStyle(
            objc2_ui_kit::UIFontTextStyleBody,
        )));
        label.setTextColor(Some(&UIColor::labelColor()));
    }
    label.setBackgroundColor(Some(&UIColor::secondarySystemBackgroundColor()));
    label.setTextAlignment(objc2_ui_kit::NSTextAlignment::Center);
    label.layer().setCornerRadius(8.);
    label.setClipsToBounds(true);
    label.sizeToFit();
    let mut frame = label.frame();
    frame.size.width += 2. * PREVIEW_PADDING;
    frame.size.height += PREVIEW_PADDING;
    label.setFrame(frame);
    let parameters = UIDragPreviewParameters::new(main_thread);
    let location = session.locationInView(view);
    let target = UIDragPreviewTarget::initWithContainer_center(
        UIDragPreviewTarget::alloc(main_thread),
        view,
        CGPoint::new(location.x, location.y),
    );
    UITargetedDragPreview::initWithView_parameters_target(
        UITargetedDragPreview::alloc(main_thread),
        &label,
        &parameters,
        &target,
    )
}
