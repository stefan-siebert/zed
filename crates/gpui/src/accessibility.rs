//! Cross-platform accessibility sidecar.
//!
//! This module is a *sidecar* in the architectural sense: it observes existing
//! GPUI structures (focus tree, hitbox tree, interactivity metadata) and
//! synthesizes an `accesskit::Tree` from them. It does **not** require
//! `Element` implementations to opt in. Elements that wish to expose
//! accessibility info attach metadata via `Interactivity` modifiers
//! (`.accessibility_label`, `.accessibility_role`, …) which are stored here.
//!
//! The platform crates (`gpui_macos`, `gpui_windows`, `gpui_linux`) own the
//! AccessKit *adapter* that talks to the OS. The adapter pulls a
//! [`TreeUpdate`] from this sidecar on demand, and pushes [`ActionRequest`]s
//! back into the dispatch pipeline.

#![cfg(feature = "accessibility")]

use crate::{ElementId, EntityId, FocusId, SharedString};
use collections::HashMap;
use std::sync::Arc;

pub use accesskit::{
    Action as AccessibilityAction, ActionRequest, NodeId as AccessibilityNodeId, Role,
    TreeUpdate,
};

/// Per-element accessibility metadata, populated by `Interactivity` modifiers.
///
/// Stored keyed by `ElementId` (for stateful elements) or `FocusId` (for
/// focusable but unkeyed elements). Elements with neither identity are not
/// individually addressable and are folded into their parent's bounds.
#[derive(Clone, Default)]
pub struct AccessibilityMetadata {
    pub role: Option<Role>,
    pub label: Option<SharedString>,
    pub description: Option<SharedString>,
    pub keyboard_shortcut: Option<SharedString>,
    pub hidden: bool,
}

/// The sidecar tree, owned by `Window`. One per window.
///
/// The tree is mutated during paint (when Interactivity attaches metadata)
/// and *consumed* by the platform adapter when the OS asks for it. We avoid
/// the per-frame `TreeUpdate` push from the original plan: instead we mark
/// the tree dirty when metadata, focus, or layout changes, and let the
/// platform layer pull a `TreeUpdate` lazily.
pub struct AccessibilityTree {
    nodes_by_focus: HashMap<FocusId, AccessibilityNodeId>,
    nodes_by_element: HashMap<ElementId, AccessibilityNodeId>,
    metadata: HashMap<AccessibilityNodeId, AccessibilityMetadata>,
    bounds: HashMap<AccessibilityNodeId, crate::Bounds<crate::Pixels>>,
    parent_of: HashMap<AccessibilityNodeId, AccessibilityNodeId>,
    next_id: u64,
    dirty: bool,
    action_dispatch: Option<Arc<dyn Fn(ActionRequest) + Send + Sync>>,
}

impl AccessibilityTree {
    pub fn new() -> Self {
        Self {
            nodes_by_focus: HashMap::default(),
            nodes_by_element: HashMap::default(),
            metadata: HashMap::default(),
            bounds: HashMap::default(),
            parent_of: HashMap::default(),
            next_id: 1,
            dirty: false,
            action_dispatch: None,
        }
    }

    /// Begin a new frame's accessibility pass. Called once per paint, before
    /// any Interactivity-driven updates.
    pub fn begin_frame(&mut self) {
        self.metadata.clear();
        self.bounds.clear();
        self.parent_of.clear();
        self.dirty = true;
    }

    /// Attach or update metadata for an element addressed by its focus handle.
    pub fn attach_focus(
        &mut self,
        focus_id: FocusId,
        metadata: AccessibilityMetadata,
        bounds: crate::Bounds<crate::Pixels>,
    ) {
        let node_id = self.id_for_focus(focus_id);
        self.metadata.insert(node_id, metadata);
        self.bounds.insert(node_id, bounds);
    }

    /// Attach or update metadata for an element addressed by its `ElementId`.
    pub fn attach_element(
        &mut self,
        element_id: &ElementId,
        metadata: AccessibilityMetadata,
        bounds: crate::Bounds<crate::Pixels>,
    ) {
        let node_id = self.id_for_element(element_id);
        self.metadata.insert(node_id, metadata);
        self.bounds.insert(node_id, bounds);
    }

    /// Returns the accesskit tree update if anything changed since the last
    /// pull, otherwise `None`. The platform adapter calls this from its
    /// callback into AccessKit.
    pub fn take_update(&mut self, focused: Option<FocusId>) -> Option<TreeUpdate> {
        if !self.dirty {
            return None;
        }
        self.dirty = false;
        // SKETCH: assemble accesskit::Node values from `self.metadata` +
        // `self.bounds`, parent pointers from `self.parent_of`, and the
        // current focus from `focused`. Returns a complete TreeUpdate.
        // Real impl will diff against a cached previous TreeUpdate to send
        // only deltas.
        todo!("assemble TreeUpdate from sidecar state")
    }

    /// Install the callback used to dispatch `ActionRequest`s back into the
    /// window's input pipeline. The platform adapter calls this once during
    /// window construction.
    pub fn set_action_dispatch(
        &mut self,
        dispatch: Arc<dyn Fn(ActionRequest) + Send + Sync>,
    ) {
        self.action_dispatch = Some(dispatch);
    }

    /// Forwards an OS-originated `ActionRequest` (e.g. VoiceOver "press") to
    /// the registered dispatcher. Called by the platform adapter.
    pub fn dispatch_action(&self, request: ActionRequest) {
        if let Some(dispatch) = self.action_dispatch.as_ref() {
            dispatch(request);
        } else {
            log::warn!(
                "accessibility action received before dispatcher was installed: {:?}",
                request.action
            );
        }
    }

    fn id_for_focus(&mut self, focus_id: FocusId) -> AccessibilityNodeId {
        if let Some(existing) = self.nodes_by_focus.get(&focus_id) {
            return *existing;
        }
        let new_id = self.allocate_id();
        self.nodes_by_focus.insert(focus_id, new_id);
        new_id
    }

    fn id_for_element(&mut self, element_id: &ElementId) -> AccessibilityNodeId {
        if let Some(existing) = self.nodes_by_element.get(element_id) {
            return *existing;
        }
        let new_id = self.allocate_id();
        self.nodes_by_element.insert(element_id.clone(), new_id);
        new_id
    }

    fn allocate_id(&mut self) -> AccessibilityNodeId {
        let id = AccessibilityNodeId(self.next_id);
        self.next_id += 1;
        id
    }
}

impl Default for AccessibilityTree {
    fn default() -> Self {
        Self::new()
    }
}

/// Trait implemented by the platform adapter (one impl per
/// `gpui_macos`/`gpui_windows`/`gpui_linux`). The window owns a boxed instance
/// and forwards lifecycle events to it.
pub trait PlatformAccessibilityAdapter: 'static {
    /// Called after `AccessibilityTree::take_update` has produced a new
    /// update, to push it to the OS.
    fn push_update(&self, update: TreeUpdate);

    /// Called when the keyboard focus changes within GPUI, so the adapter
    /// can notify the OS.
    fn focus_changed(&self, focus: Option<FocusId>);

    /// Called when the OS-level window gains or loses keyboard focus, so the
    /// adapter can suppress unnecessary work while not active.
    fn window_active_changed(&self, active: bool);
}
