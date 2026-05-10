//! macOS AccessKit adapter.
//!
//! Glues the GPUI sidecar (`gpui::accessibility::AccessibilityTree`) to
//! `accesskit_macos::SubclassingAdapter`. The adapter dynamically subclasses
//! the host NSView and registers the four NSAccessibility selectors we'd
//! otherwise have to write by hand (`accessibilityChildren`,
//! `accessibilityHitTest:`, `accessibilityFocusedUIElement`,
//! `isAccessibilityElement`). We only need to provide:
//!
//! 1. An [`accesskit::ActivationHandler`] that returns the initial tree
//!    snapshot the first time AT (e.g. VoiceOver) probes the application.
//! 2. An [`accesskit::ActionHandler`] that turns OS-originated
//!    `ActionRequest`s into GPUI input events. Hybrid policy:
//!    - `Action::Click` → synthetic `MouseDownEvent` + `MouseUpEvent` at
//!      the target node's hitbox center (preserves modifier semantics and
//!      hit-test ordering).
//!    - `Action::Focus`, `ScrollIntoView`, `Increment`, `Decrement`, … →
//!      direct API call (no synthetic event makes sense for these).

#![cfg(all(target_os = "macos", feature = "accessibility"))]

use accesskit::{Action, ActionHandler, ActionRequest, ActivationHandler, TreeUpdate};
use accesskit_macos::SubclassingAdapter;
use cocoa::base::id;
use gpui::accessibility::{AccessibilityTree, PlatformAccessibilityAdapter};
use gpui::FocusId;
use parking_lot::Mutex;
use std::sync::Arc;

/// Owns the `SubclassingAdapter` and the shared sidecar handle.
///
/// The lifetime contract: the adapter is created with the `NSView*` (our
/// `GPUIView`) as its host, and dropped before the view is dropped. The
/// shared sidecar is held behind a `Mutex` so AccessKit callbacks (running
/// on the AppKit main thread) can read it without going through the GPUI
/// `App` borrow. See `gpui::accessibility` for the threading contract.
pub(crate) struct MacAccessibility {
    adapter: Mutex<SubclassingAdapter>,
    tree: Arc<Mutex<AccessibilityTree>>,
}

impl MacAccessibility {
    /// Construct an adapter bound to the given GPUIView.
    ///
    /// `gpui_event_callback` is the existing event-callback closure on
    /// `MacWindowState`. We use it to inject synthetic mouse events for
    /// `Action::Click` so that all click handling — registered or
    /// AT-originated — flows through the same dispatch path.
    ///
    /// # Safety
    ///
    /// `view` must be a valid, retained NSView pointer that lives at least
    /// as long as the returned adapter.
    pub unsafe fn new(
        view: id,
        tree: Arc<Mutex<AccessibilityTree>>,
        action_dispatch: Arc<dyn Fn(ActionRequest) + Send + Sync>,
    ) -> Self {
        let activation = ActivationBridge {
            tree: tree.clone(),
        };
        let action = ActionBridge {
            dispatch: action_dispatch.clone(),
        };
        // SAFETY: caller guarantees `view` is a valid retained NSView.
        let adapter = unsafe {
            SubclassingAdapter::new(view as *mut std::ffi::c_void, activation, action)
        };

        // Install the action dispatcher on the sidecar so that test-only
        // synthetic actions can route through the same channel.
        tree.lock().set_action_dispatch(action_dispatch);

        Self {
            adapter: Mutex::new(adapter),
            tree,
        }
    }
}

impl PlatformAccessibilityAdapter for MacAccessibility {
    fn flush(&self) {
        let snapshot = {
            let mut sidecar = self.tree.lock();
            sidecar.take_update()
        };
        let Some(update) = snapshot else { return };
        let queued = self.adapter.lock().update_if_active(|| update);
        if let Some(events) = queued {
            events.raise();
        }
    }

    fn focus_changed(&self, _focus: Option<FocusId>) {
        // The sidecar already records focus; flushing emits a TreeUpdate
        // with the new focus, which is what AccessKit needs.
        self.flush();
    }

    fn window_active_changed(&self, active: bool) {
        let queued = self.adapter.lock().update_view_focus_state(active);
        if let Some(events) = queued {
            events.raise();
        }
    }
}

/// Bridge from AccessKit's `ActivationHandler` callback (called the first
/// time AT probes the app) into the sidecar's snapshot path.
struct ActivationBridge {
    tree: Arc<Mutex<AccessibilityTree>>,
}

impl ActivationHandler for ActivationBridge {
    fn request_initial_tree(&mut self) -> Option<TreeUpdate> {
        Some(self.tree.lock().snapshot())
    }
}

/// Bridge from AccessKit's `ActionHandler` callback (VoiceOver, hardware
/// switch control, etc.) into the GPUI input pipeline.
///
/// Click-like actions are turned into synthetic `PlatformInput::MouseDown` +
/// `PlatformInput::MouseUp` at the target node's hitbox center. Other
/// actions (Focus, ScrollIntoView, Increment, Decrement, …) call into GPUI's
/// API directly via the registered dispatch closure — there is no sensible
/// synthetic event for those.
///
/// Both branches are routed through the same `dispatch` closure, which is
/// a thin wrapper installed by `MacWindow::open` that knows how to talk to
/// `event_callback` and the focus map.
struct ActionBridge {
    dispatch: Arc<dyn Fn(ActionRequest) + Send + Sync>,
}

impl ActionHandler for ActionBridge {
    fn do_action(&mut self, request: ActionRequest) {
        // The synth-vs-direct policy lives in the dispatch closure (in
        // window.rs) so it has direct access to `event_callback` and the
        // focus dispatcher. This bridge just forwards.
        match request.action {
            Action::Click
            | Action::Focus
            | Action::Blur
            | Action::ScrollIntoView
            | Action::Increment
            | Action::Decrement
            | Action::SetValue
            | Action::ShowContextMenu => (self.dispatch)(request),
            // Drop actions we explicitly don't support yet so AT clients
            // get a no-op rather than a crash. Logging is at debug to avoid
            // spam from clients that probe the full action set.
            other => {
                log::debug!(
                    "accessibility action {:?} not yet implemented for node {:?}",
                    other,
                    request.target,
                );
            }
        }
    }
}
