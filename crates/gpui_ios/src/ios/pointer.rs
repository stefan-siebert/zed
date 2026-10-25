//! Trackpad and mouse: the iPad pointer as GPUI mouse events.
//!
//! An app that declares `UIApplicationSupportsIndirectInputEvents` in its
//! Info.plist gets a pointer click as a touch of type `indirectPointer`;
//! without it UIKit turns the click into a finger touch, which GPUI's touch
//! recognizer handles as a tap. Measured with XCUITest's pointer on the
//! iPadOS 27.0 simulator: primary and secondary clicks both reach
//! `touchesBegan`, told apart by the event's button mask, with a tap count
//! that counts a double click; the event's modifier flags stayed empty while
//! XCUITest held ⌘ or ⇧, though the hardware keyboard reported the key.
//!
//! So a pointer touch becomes mouse down, move and up with that button, the
//! click count and the modifiers of event and keyboard together, as on macOS,
//! and skips GPUI's touch gestures. A pointer that moves with no button down
//! reaches only a hover recognizer, which makes `MouseMove` without a button
//! and, when the pointer leaves the view, `MouseExited`. (XCUITest's `hover()`
//! reached no recognizer in the simulator; the recognizer began and changed
//! around its clicks, and was cancelled as each click began.)
//!
//! The pointer's shape follows `set_cursor_style` through a
//! `UIPointerInteraction`: a beam for text, the system pointer otherwise —
//! iPadOS has no resize, hand or crosshair pointer.

use std::cell::Cell;

use gpui::{
    CursorStyle, Modifiers, MouseButton, MouseDownEvent, MouseExitEvent, MouseMoveEvent,
    MouseUpEvent, PlatformInput, Point, px,
};
use objc2::{
    MainThreadMarker, MainThreadOnly, Message,
    rc::Retained,
    runtime::{AnyObject, ProtocolObject},
    sel,
};
use objc2_ui_kit::{
    UIAxis, UIEvent, UIEventButtonMask, UIGestureRecognizerState, UIHoverGestureRecognizer,
    UIInteraction, UIPointerInteraction, UIPointerInteractionDelegate, UIPointerRegion,
    UIPointerShape, UIPointerStyle, UITouch, UITouchPhase, UITouchType, UIView,
};

use super::events::touch_location_in_view;
use super::keyboard::modifiers;
use super::window::IosWindowState;

/// The length of the text beam, in points. GPUI does not say how tall the
/// text under the pointer is; this suits body text.
const BEAM_LENGTH: f64 = 18.;

/// Per-window pointer state.
#[derive(Default)]
pub(super) struct Pointer {
    /// The button of the pointer touch in progress.
    pressed: Cell<Option<MouseButton>>,
    hovered: Cell<bool>,
}

impl Pointer {
    pub(super) fn is_hovered(&self) -> bool {
        self.hovered.get()
    }
}

/// Adds the hover recognizer and the pointer interaction to `view`.
/// `target` implements `handlePointerHover:`.
pub(super) fn install(
    view: &UIView,
    target: &AnyObject,
    delegate: &ProtocolObject<dyn UIPointerInteractionDelegate>,
    main_thread: MainThreadMarker,
) -> Retained<UIPointerInteraction> {
    // SAFETY: `target` implements the action and the view owns the recognizer.
    let hover = unsafe {
        UIHoverGestureRecognizer::initWithTarget_action(
            UIHoverGestureRecognizer::alloc(main_thread),
            Some(target),
            Some(sel!(handlePointerHover:)),
        )
    };
    view.addGestureRecognizer(&hover);

    let interaction = UIPointerInteraction::initWithDelegate(
        UIPointerInteraction::alloc(main_thread),
        Some(delegate),
    );
    view.addInteraction(ProtocolObject::<dyn UIInteraction>::from_ref(&*interaction));
    interaction
}

/// Delivers a pointer touch as a mouse event. Returns `false` for any other
/// touch, which belongs to GPUI's touch recognizer.
pub(super) fn handle_touch(
    window: &IosWindowState,
    touch: &UITouch,
    event: Option<&UIEvent>,
    view: &UIView,
) -> bool {
    if touch.r#type() != UITouchType::IndirectPointer {
        return false;
    }
    let position = touch_location_in_view(touch, view);
    let modifiers = event.map_or_else(Modifiers::default, |event| modifiers(event.modifierFlags()))
        | window.modifiers();
    let pointer = window.pointer();
    let click_count = touch.tapCount().max(1);
    let input = match touch.phase() {
        UITouchPhase::Began => {
            let secondary = event
                .is_some_and(|event| event.buttonMask().contains(UIEventButtonMask::Secondary));
            let button = if secondary {
                MouseButton::Right
            } else {
                MouseButton::Left
            };
            pointer.pressed.set(Some(button));
            PlatformInput::MouseDown(MouseDownEvent {
                button,
                position,
                modifiers,
                click_count,
                first_mouse: false,
            })
        }
        UITouchPhase::Moved | UITouchPhase::Stationary => {
            PlatformInput::MouseMove(MouseMoveEvent {
                position,
                pressed_button: pointer.pressed.get(),
                modifiers,
            })
        }
        _ => {
            let Some(button) = pointer.pressed.take() else {
                return true;
            };
            PlatformInput::MouseUp(MouseUpEvent {
                button,
                position,
                modifiers,
                click_count,
            })
        }
    };
    window.set_pointer_input(position, modifiers);
    window.dispatch_input(input);
    true
}

/// Delivers a step of the hover recognizer.
pub(super) fn handle_hover(
    window: &IosWindowState,
    recognizer: &UIHoverGestureRecognizer,
    view: &UIView,
) {
    let location = recognizer.locationInView(Some(view));
    let position = Point::new(px(location.x as f32), px(location.y as f32));
    let modifiers = modifiers(recognizer.modifierFlags()) | window.modifiers();
    let pointer = window.pointer();
    match recognizer.state() {
        UIGestureRecognizerState::Began | UIGestureRecognizerState::Changed => {
            window.show_cursor();
            if !pointer.hovered.replace(true) {
                window.hover_status_changed(true);
            }
            window.set_pointer_input(position, modifiers);
            window.dispatch_input(PlatformInput::MouseMove(MouseMoveEvent {
                position,
                pressed_button: None,
                modifiers,
            }));
        }
        // The pointer left the view.
        UIGestureRecognizerState::Ended => {
            if pointer.hovered.replace(false) {
                window.hover_status_changed(false);
            }
            window.dispatch_input(PlatformInput::MouseExited(MouseExitEvent {
                position,
                pressed_button: None,
                modifiers,
            }));
        }
        // A click began: the pointer is still there, and its touch reports
        // the position from now on (cancelled at (0, 0), measured).
        _ => {}
    }
}

/// The region the pointer interaction customizes: the whole view whenever
/// the pointer has a shape of its own, else none, which leaves the system
/// pointer.
pub(super) fn region(
    cursor: CursorStyle,
    hidden: bool,
    default_region: &UIPointerRegion,
) -> Option<Retained<UIPointerRegion>> {
    let customized = hidden
        || matches!(
            cursor,
            CursorStyle::IBeam | CursorStyle::IBeamCursorForVerticalLayout
        );
    customized.then(|| default_region.retain())
}

/// The pointer style for `cursor`.
pub(super) fn style(
    cursor: CursorStyle,
    hidden: bool,
    main_thread: MainThreadMarker,
) -> Option<Retained<UIPointerStyle>> {
    if hidden {
        return Some(UIPointerStyle::hiddenPointerStyle(main_thread));
    }
    let axis = match cursor {
        CursorStyle::IBeam => UIAxis::Vertical,
        CursorStyle::IBeamCursorForVerticalLayout => UIAxis::Horizontal,
        _ => return None,
    };
    let shape = UIPointerShape::beamWithPreferredLength_axis(BEAM_LENGTH, axis, main_thread);
    Some(UIPointerStyle::styleWithShape_constrainedAxes(
        &shape,
        UIAxis::empty(),
    ))
}
