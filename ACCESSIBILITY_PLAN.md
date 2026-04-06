# Cross-Platform Accessibility Integration Plan for GPUI (Elane)

Apple requires macOS applications to be accessible—in particular, they must support VoiceOver, standard keyboard navigation, and structural semantics. Currently, the GPUI fork used by Elane **does not contain any native accessibility bindings** (e.g., `NSAccessibility` protocols in `gpui_macos` are missing). Without this, Apple's App Store reviewers will reject Elane.

To fix this and satisfy App Store requirements across all platforms (macOS, Windows, Linux), we need to retrofit accessibility into GPUI. The most industry-standard cross-platform way to accomplish this in Rust GUI frameworks is via the **AccessKit** library, which is used by egui, wgpu, and winit.

## Effort Estimation

### 1. GPUI Core Adjustments (Effort: High, ~2-3 weeks)
* **Element-Tree Mapping:** The `Element` trait in GPUI must be extended so that every UI element (buttons, lists) generates an accessibility node (`accesskit::Node`).
* **AccessKit Tree Updates:** GPUI must generate an `accesskit::TreeUpdate` on each frame/layout pass to tell the OS what changed (focus, new elements).
* **Action Requests:** GPUI needs to receive `accesskit::ActionRequest`s (e.g., "click this element" from VoiceOver) and route them to the correct UI component.

### 2. Platform Bindings (Effort: Medium, ~1-2 weeks)
Once GPUI Core generates the tree, we need to pass it to the host OS.
* **macOS (`gpui_macos`):** Integrate `accesskit_macos` into `GPUIView` (Objective-C). Forward `NSAccessibility` calls to the adapter.
* **Windows (`gpui_windows`):** Integrate `accesskit_windows`. Hook into the `HWND` message loop to intercept `WM_GETOBJECT`.
* **Linux (`gpui_linux`):** Integrate `accesskit_unix` (AT-SPI) via X11/Wayland event loops.

### 3. Elane UI Components (Effort: Low, ~1 week)
Once GPUI supports it, Elane's UI components need to be annotated:
* Add `.accessibility_role(Role::Button)` and `.accessibility_label("Delete")` to clickable elements.
* Annotate file lists/trees correctly so screen readers understand the hierarchy.

**Total Effort:** ~4 to 6 weeks for a robust, cross-platform implementation.

## Implementation Steps (macOS Focus for App Store)
As a starting point for macOS App Store compliance:
1. Add `accesskit` to `gpui`.
2. Implement basic role and label tracking in `Element`s.
3. Add `accesskit_macos` to `gpui_macos`.
4. Initialize `accesskit_macos::Adapter` in `window.rs` (`GPUIView`).
5. Route `accessibilityChildren` and other NSAccessibility methods to the adapter.
6. Verify with Xcode's Accessibility Inspector and macOS VoiceOver (`Cmd + F5`).
