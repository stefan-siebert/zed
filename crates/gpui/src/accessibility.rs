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

use crate::{Bounds, ElementId, FocusId, Pixels, SharedString};
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
    /// Semantic role exposed to AT (Button, Link, Checkbox, …).
    pub role: Option<Role>,
    /// Short text spoken by AT when the element gains focus.
    pub label: Option<SharedString>,
    /// Extended text spoken after the label.
    pub description: Option<SharedString>,
    /// Human-readable keyboard shortcut hint, e.g. "Cmd-S".
    pub keyboard_shortcut: Option<SharedString>,
    /// When `true`, this element is omitted from the AT tree entirely.
    pub hidden: bool,
}

/// Root node id used when no element claims focus. Hard-coded to 1 so that
/// real element node ids start above it without conflict.
const ROOT_NODE_ID: AccessibilityNodeId = AccessibilityNodeId(1);

/// The sidecar tree, owned by `Window`. One per window.
///
/// The tree is mutated during paint (when Interactivity attaches metadata)
/// and *consumed* by the platform adapter when the OS asks for it. We don't
/// push per-frame TreeUpdates; the adapter pulls when AccessKit needs them
/// (lazy initial tree, focus-changed events, action-induced re-reads).
pub struct AccessibilityTree {
    nodes_by_focus: HashMap<FocusId, AccessibilityNodeId>,
    nodes_by_element: HashMap<ElementId, AccessibilityNodeId>,
    metadata: HashMap<AccessibilityNodeId, AccessibilityMetadata>,
    bounds: HashMap<AccessibilityNodeId, Bounds<Pixels>>,
    children_of: HashMap<AccessibilityNodeId, Vec<AccessibilityNodeId>>,
    next_id_counter: u64,
    dirty: bool,
    focused: Option<AccessibilityNodeId>,
    action_dispatch: Option<Arc<dyn Fn(ActionRequest) + Send + Sync>>,
}

impl AccessibilityTree {
    /// Create an empty tree. One per window; constructed in `Window::new`.
    pub fn new() -> Self {
        Self {
            nodes_by_focus: HashMap::default(),
            nodes_by_element: HashMap::default(),
            metadata: HashMap::default(),
            bounds: HashMap::default(),
            children_of: HashMap::default(),
            // Start above ROOT_NODE_ID so allocated ids never collide.
            next_id_counter: 2,
            dirty: false,
            focused: None,
            action_dispatch: None,
        }
    }

    /// Begin a new frame's accessibility pass. Called once per paint, before
    /// any Interactivity-driven updates. Bounds and metadata are cleared;
    /// the FocusId↔NodeId mapping is preserved across frames so that
    /// AccessKit's per-node identity is stable.
    pub fn begin_frame(&mut self) {
        self.metadata.clear();
        self.bounds.clear();
        self.children_of.clear();
        self.dirty = true;
    }

    /// Attach or update metadata for an element addressed by its focus handle.
    pub fn attach_focus(
        &mut self,
        focus_id: FocusId,
        metadata: AccessibilityMetadata,
        bounds: Bounds<Pixels>,
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
        bounds: Bounds<Pixels>,
    ) {
        let node_id = self.id_for_element(element_id);
        self.metadata.insert(node_id, metadata);
        self.bounds.insert(node_id, bounds);
    }

    /// Record the currently keyboard-focused element. Called from the
    /// window's focus dispatcher.
    pub fn set_focus(&mut self, focus_id: Option<FocusId>) {
        let new_focused = focus_id.and_then(|id| self.nodes_by_focus.get(&id).copied());
        if new_focused != self.focused {
            self.focused = new_focused;
            self.dirty = true;
        }
    }

    /// Look up the bounds of a node by its AccessKit NodeId. Used by the
    /// platform adapter to synthesize `MouseDown`/`MouseUp` events at the
    /// hitbox center for `Action::Click` requests.
    pub fn bounds_for(&self, node_id: AccessibilityNodeId) -> Option<Bounds<Pixels>> {
        self.bounds.get(&node_id).copied()
    }

    /// Resolve a NodeId back to its FocusId, when one was registered.
    /// Used by the platform adapter to dispatch `Action::Focus` directly
    /// to GPUI's focus system.
    pub fn focus_id_for(&self, node_id: AccessibilityNodeId) -> Option<FocusId> {
        self.nodes_by_focus
            .iter()
            .find_map(|(focus_id, id)| (*id == node_id).then_some(*focus_id))
    }

    /// Returns a complete TreeUpdate snapshot if anything changed since the
    /// last pull. Phase-1 sends full snapshots — diffing is a later
    /// optimization. Returns `None` if nothing has changed.
    pub fn take_update(&mut self) -> Option<TreeUpdate> {
        if !self.dirty {
            return None;
        }
        self.dirty = false;
        Some(self.snapshot())
    }

    /// Build a TreeUpdate that always contains the full tree, regardless of
    /// dirty state. Used for `request_initial_tree` where the adapter needs
    /// a self-contained snapshot even if nothing has changed since
    /// construction.
    pub fn snapshot(&self) -> TreeUpdate {
        let mut nodes: Vec<(AccessibilityNodeId, accesskit::Node)> =
            Vec::with_capacity(self.metadata.len() + 1);

        let mut child_ids: Vec<AccessibilityNodeId> = Vec::new();
        for (node_id, metadata) in &self.metadata {
            if metadata.hidden {
                continue;
            }
            let role = metadata.role.unwrap_or(Role::GenericContainer);
            let mut node = accesskit::Node::new(role);
            if let Some(label) = metadata.label.as_ref() {
                node.set_label(label.to_string());
            }
            if let Some(description) = metadata.description.as_ref() {
                node.set_description(description.to_string());
            }
            if let Some(shortcut) = metadata.keyboard_shortcut.as_ref() {
                node.set_keyboard_shortcut(shortcut.to_string());
            }
            if let Some(bounds) = self.bounds.get(node_id) {
                node.set_bounds(accesskit::Rect {
                    x0: f64::from(bounds.origin.x.0),
                    y0: f64::from(bounds.origin.y.0),
                    x1: f64::from(bounds.origin.x.0 + bounds.size.width.0),
                    y1: f64::from(bounds.origin.y.0 + bounds.size.height.0),
                });
            }
            // Phase-1 default actions: every visible role-bearing node
            // accepts Click + Focus. Element-specific action sets (e.g.
            // Increment/Decrement on sliders) come in a follow-up commit
            // alongside `gpui-component`.
            node.add_action(AccessibilityAction::Click);
            node.add_action(AccessibilityAction::Focus);
            nodes.push((*node_id, node));
            child_ids.push(*node_id);
        }

        let mut root = accesskit::Node::new(Role::Window);
        root.set_children(child_ids);
        nodes.push((ROOT_NODE_ID, root));

        TreeUpdate {
            nodes,
            tree: Some(accesskit::Tree::new(ROOT_NODE_ID)),
            focus: self.focused.unwrap_or(ROOT_NODE_ID),
        }
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

    /// Forwards an OS-originated `ActionRequest` to the registered
    /// dispatcher. Called by the platform adapter from its `do_action`.
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
        let id = AccessibilityNodeId(self.next_id_counter);
        self.next_id_counter += 1;
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
    /// Called when GPUI has new accessibility state to push (e.g. after a
    /// frame in which Interactivity metadata changed). The adapter typically
    /// calls `Adapter::update_if_active(|| tree.snapshot())` and raises any
    /// returned `QueuedEvents`.
    fn flush(&self);

    /// Called when keyboard focus moves inside GPUI.
    fn focus_changed(&self, focus: Option<FocusId>);

    /// Called when the OS-level window gains or loses keyboard focus.
    fn window_active_changed(&self, active: bool);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{point, size, Pixels};

    fn metadata_with_label(role: Role, label: &str) -> AccessibilityMetadata {
        AccessibilityMetadata {
            role: Some(role),
            label: Some(label.into()),
            description: None,
            keyboard_shortcut: None,
            hidden: false,
        }
    }

    fn rect(x: f32, y: f32, w: f32, h: f32) -> Bounds<Pixels> {
        Bounds {
            origin: point(Pixels(x), Pixels(y)),
            size: size(Pixels(w), Pixels(h)),
        }
    }

    #[test]
    fn snapshot_includes_root_and_visible_children_skips_hidden() {
        let mut tree = AccessibilityTree::new();
        tree.begin_frame();

        let element_id_button: ElementId = "button-save".into();
        let element_id_decoration: ElementId = "decoration-divider".into();
        tree.attach_element(
            &element_id_button,
            metadata_with_label(Role::Button, "Save"),
            rect(0.0, 0.0, 80.0, 24.0),
        );
        let mut hidden = AccessibilityMetadata::default();
        hidden.hidden = true;
        tree.attach_element(&element_id_decoration, hidden, rect(0.0, 30.0, 80.0, 1.0));

        let update = tree.take_update().expect("dirty after attach");
        assert!(update.tree.is_some());

        let labels: Vec<String> = update
            .nodes
            .iter()
            .filter_map(|(_, node)| node.label().map(|n| n.to_string()))
            .collect();
        assert_eq!(labels, vec!["Save".to_string()]);

        let root = update
            .nodes
            .iter()
            .find(|(id, _)| *id == ROOT_NODE_ID)
            .expect("root present")
            .1
            .clone();
        assert_eq!(root.children().len(), 1);

        // Second take_update without changes returns None.
        assert!(tree.take_update().is_none());
    }

    #[test]
    fn focus_defaults_to_root_when_unset() {
        let mut tree = AccessibilityTree::new();
        tree.begin_frame();
        let element_id: ElementId = "btn".into();
        tree.attach_element(
            &element_id,
            metadata_with_label(Role::Button, "Go"),
            rect(0.0, 0.0, 10.0, 10.0),
        );
        let update = tree.take_update().expect("dirty");
        assert_eq!(update.focus, ROOT_NODE_ID);
    }
}
