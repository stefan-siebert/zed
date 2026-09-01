# GPUI Fork — Patches & Changes

**Fork:** `stefan-siebert/zed`, branch `gpui-mcp-patches-v2`
**Built from:** local checkout `../gpui-fork` (overrides the git dep via Cargo `[patch]` in Elane's `Cargo.toml`).
**Baseline:** upstream Zed merged at commit `ce48461e` (merge commit `53bbec49`, 2026-09-01).

This file inventories the **custom commits on top of upstream**. Everything else on the branch is upstream PRs pulled in by merges.

## How to regenerate this list

```bash
cd ../gpui-fork
# Custom (non-PR-numbered) commits vs the fork mirror:
git log --oneline --no-merges $(git merge-base HEAD upstream/main)..gpui-mcp-patches-v2
# Net diff of all custom patches vs the upstream merge point:
git diff --stat $(git merge-base HEAD upstream/main)..gpui-mcp-patches-v2
```

Upstream PRs are tagged `(#NNNNN)`; custom patches use conventional-commit style without a PR number.

---

## Summary

**65 custom commits**, net **+6,693 / −379 lines across 68 files** vs the upstream merge point (`ce48461e`, measured 2026-09-01).

Both figures come straight from the commands above; re-run them after every
upstream merge, and move the baseline commit with them. Counting against
`origin/main` (the fork mirror) instead of the merge base silently folds in
upstream commits that carry no PR number.

⚠️ These forks are load-bearing (see Elane `CLAUDE.md`). `cargo update` against upstream will break the shader, inspector, drag, and clipboard work.

---

## 1. MCP / UI Inspector APIs (original purpose of the fork)

| Commit | Change |
|---|---|
| `d8b7b8b3` | Public APIs for MCP UI inspection and event dispatch (dispatch wrappers, inspector entry points) |
| `541898a1` | Capture painted text content for the MCP inspector |
| `78866100` | Deduplicate inspector text content — assign to smallest container |
| `48d68301` | **`Window::set_a11y_force_active`.** Upstream builds the AccessKit tree only while assistive technology is attached, which leaves it unreadable to anything that wants to *check* accessibility rather than consume it — a test, the MCP inspector, a CI run. Adds `force_enabled` to the per-window `A11y` and ORs it into `sync_active_flag`, so `Application::new_inaccessible` still wins and the default is unchanged. Takes effect from the next frame: the flag for the frame being painted is latched before the first node is pushed, and the builder late to that stack would push and pop unevenly. |
| `d869efe7` | **`InspectorElementInfo::accesskit_node_id`.** The only exact join between the inspector and the accessibility tree. A node records the leaf of its element id and its source location, and four title-bar buttons of the same widget share both — matching on that pair picks one at random. gpui already derives a node’s AccessKit id from the whole `GlobalElementId`, so the inspector now reports that id and the two sides join by identity. Computed inside gpui and only read outside it, so nothing downstream depends on `DefaultHasher` being stable. |

## 2. Per-element backdrop blur (macOS)

| Commit | Change |
|---|---|
| _this commit_ | **`BackdropBlur` primitive + Metal implementation.** GPUI had no backdrop-filter primitive at all: both shader paths (Metal's `quad_fragment` `effect_type` branches and the WGSL `CustomShader` contract) bind no texture, so neither can read what is already painted behind an element. Adds `BackdropBlur` to `Scene` (`bounds`, `content_mask`, `corner_radii`, `blur_radius`, `tint`), `Window::paint_backdrop_blur`, and `PlatformWindow::supports_backdrop_blur` (default `false`). `PrimitiveKind::BackdropBlur` sits **between `Shadow` and `Quad`** so a backdrop at the same draw order paints under the quad that requested it, while every existing kind keeps its relative order. macOS implements it: a dual-Kawase chain (Bjorge, ARM, SIGGRAPH 2015) on a half-resolution ping-pong pair, composited through `quad_sdf` for rounded corners. wgpu and DirectX get a no-op arm and keep reporting `false`. |

Two things this required that are easy to regress:

1. **`layer.set_framebuffer_only(false)` is now unconditional** (`metal_renderer.rs`),
   previously only under `test-support`. The blur samples the drawable it is
   rendering into, so the drawable must be readable. Apple defaults this the
   other way because it forgoes display-path optimisations.
2. **The composite pipeline must not blend** in the usual source-over sense
   (`build_path_sprite_pipeline_state`, i.e. `One` / `1-SrcAlpha`). The fragment
   shader has already composited the blurred destination itself; source-over
   would count the sharp original twice.

Mechanically it mirrors the existing path rendering: the batch loop ends the
encoder, runs the blur into scratch textures, and reopens the encoder on the
same target with `MTLLoadAction::Load`.

**Cost (measured 2026-07-29, GPU wall time from `GPUStartTime`/`GPUEndTime`):**
the chain adds ~0.35 ms per frame on a 900x600 window and ~0.52 ms on
1800x1200, roughly doubling GPU frame time while a blurred surface is on
screen. It originally ran over the whole viewport, which made the cost a
function of window size rather than of how much was being blurred (+0.99 ms at
1800x1200 for a dialog covering 2.6% of it); `blur_region` now restricts every
pass to the union of the frame's blur rects, grown by the blur's reach. That
halves the cost on large windows and barely moves it on small ones, where the
rect is most of the viewport anyway. What remains is fixed pass overhead — the
splits and the full-size render-target load/store — which the region cannot
touch.

**Scope:** the blur reads the window's render target, so it frosts sibling GPUI
elements — not the desktop, which is never in that target and stays the
WindowServer's job (see the colorless-blur patch below). The two compose because
the shader preserves the target's alpha.

## 3. Custom shaders / visual effects (largest feature area)

| Commit | Change |
|---|---|
| `a9093714` | Per-element quad effects + glyph-based text glow |
| `cd841350` | Declarative `text_glow()` style property |
| `ac6e965d` | Noise, vignette, and shimmer quad effects |
| `e3fdf01f` | Custom shader primitives (Phase 5) |
| `bb228f48` | Custom shader support for Windows and Web platforms |
| `c31d1826` | DirectX runtime for custom shaders (Windows) |
| `f4828a22` | Restore `CustomShaders` arm in macOS metal renderer match |
| `7b4ffb22` | Fix Point/Size field access in vignette & shimmer effects |
| `ec534b9e` | Custom-shader instances: clear per frame + 16-byte instance stride |
| _this commit_ | **Metal runtime for custom shaders (macOS).** The last backend without one: `register_custom_shader` returned `None` on macOS, so no `PrimitiveBatch::CustomShaders` was ever produced and the match arm was an empty `{}`. Elane's treemap cushions and its About-dialog backdrop therefore ran on the flat-fill fallback on the fork's own development platform. New `gpui_macos/src/metal_custom_shader.rs` mirrors `directx_custom_shader.rs`: the caller's `custom_effect` fragment is wrapped in the same WGSL module the other two backends use, translated to MSL with `naga` (new dependency, `wgsl-in` + `msl-out`), compiled through `newLibraryWithSource:`, and cached per `CustomShaderId`. Instances ride the existing per-frame instance buffer; a batch binds the whole array and passes `range.start` as the base instance, since Metal's `[[instance_id]]` counts from it. Blending is premultiplied, matching wgpu/DirectX rather than gpui's own straight-alpha pipelines. Three things to preserve across merges: naga demands a `sizes_buffer` for *any* runtime-sized array whatever the bounds-check policy says, so `_mslBufferSizes` is bound at `buffer(3)`; the quad is a four-vertex triangle strip derived from `[[vertex_id]]`, so this is the one pipeline here that binds no vertex buffer; and the module carries three headless pixel tests, one of which fails if the base instance is dropped. |
| `0f18336c` | **Text glow on macOS.** `gpui_macos`'s CoreGraphics rasterizer ignored `RenderGlyphParams::embolden`, so glow glyphs rasterized as plain sharp glyphs painted underneath the foreground pass — fully occluded, i.e. `text_glow()` silently did nothing on macOS (Elane titlebar wordmark). The mask-space glow post-processing (`glow_padding_pixels` / `embolden_alpha_mask` / `blur_alpha_mask`), previously duplicated across `gpui_windows/direct_write.rs` and `gpui_wgpu/cosmic_text_system.rs`, moved to shared `gpui/src/text_system/glow_mask.rs` (+ unit tests); both backends now import it and `gpui_macos/text_system.rs` applies it (padded `raster_bounds`, dilate + blur on the CG alpha mask, emoji path skipped). Also: `gpui`'s dev-dependency on `gpui_platform` now enables `runtime_shaders` so `cargo test -p gpui` builds on macOS without the Xcode Metal Toolchain component. |

New/large files: `gpui_windows/src/directx_custom_shader.rs` (+419), `gpui_macos/src/metal_custom_shader.rs` (+500), `gpui_wgpu/src/wgpu_renderer.rs` (+464), `shaders.wgsl`, `shaders.metal`.

**`ec534b9e` — two bugs surfaced by the first heavy real use of custom shaders**
(Elane's disk-usage treemap, which emits hundreds of custom-shader quads per
frame). Both must survive upstream merges:
1. `Scene::clear()` cleared every primitive vec except `custom_shaders`, so
   instances accumulated every frame until the instance buffer overflowed
   (`E_INVALIDARG`, "scene too large", custom count growing without bound).
2. The DirectX backend exposes each per-batch sub-range of the instance buffer
   as a raw `ByteAddressBuffer` SRV whose `FirstElement` must be 4-word/16-byte
   aligned. `CustomShaderInstance` was 104 bytes (26 words), so a batch starting
   at an odd instance offset (draw-order interleaving with text splits one run
   into sub-batches at arbitrary offsets) made `FirstElement` unaligned and
   `CreateShaderResourceView` failed. Padded the instance to 112 bytes (28
   words); matching `pad: vec2<f32>` added to the DirectX + wgpu WGSL templates.
   `custom_effect` still receives `params: array<f32,16>` unchanged.

## 3. Screenshot / render-to-image (inspector & screenshots)

| Commit | Change |
|---|---|
| `a820fc97` | `render_to_image` for Windows DirectX renderer |
| `d8d06683` | `render_to_image` for wgpu renderer (Linux screenshot support) |
| `c4724161` | Ungate `render_to_image` on macOS + clean stale `NSBeep` unsafe |
| `655b9091` | wgpu `render_to_image`: derive `premultiplied_alpha` from the surface alpha mode like `draw()` — the hardcoded 0 fed straight-alpha shader output into premultiplied-blending pipelines and bleached the whole offscreen frame toward white on PreMultiplied surfaces (inspector screenshots looked washed out while the live window was correct) |

## 4. Native drag-and-drop

| Commit | Change |
|---|---|
| `c9833333` | Native drag-and-drop for Linux/Wayland |
| `0d9e0d0c` | Windows native drag image DPI scaling + cursor offset sync |
| `0998e6f5` | **Kept alongside upstream's own outbound-drag support.** The 2026-08-04 merge brought `c7aea6cbb` (`wl_data_source` outbound drags), `f52fd9ac4` (macOS file drag-out) and the `AnyDrag::external_payload_source` / `Interactivity::external_drag_payload` API. That path promotes an internal drag automatically when the pointer leaves the viewport, but hands the platform only `FileDragPaths` — the drag image is the platform's. Ours (`Window::start_native_drag` + `AnyDrag::is_external`) lets the caller pass a rendered icon, which Elane's file table uses (`file_table.rs`: `render_drag_icon` + `NativeDragMode`). Both are live; on Wayland they are separated by a third dispatch userdata, `DataSourceKind::NativeDrag`, so `wl_data_source` events route to the right one without comparing object ids. **If Elane ever gives up the custom drag image, delete our half and use upstream's — carrying both is the cost of that image.** |
| `7080cc83` | Suppress unused `Result` warning for `SetForegroundWindow` |

New file: `gpui_windows/src/native_drag.rs` (+522).

## 5. Clipboard

| Commit | Change |
|---|---|
| `992c6958` | Write `ExternalPaths` as native file references on all platforms (macOS/Windows/Wayland/X11) |

## 6. Windows keyboard handling

| Commit | Change |
|---|---|
| `6d13b23a` | Keep unshifted key + shift modifier in `get_keystroke_key` |
| `01ff34dd` | Drop shift modifier on keybinding side after shifted-key substitution |

## 6b. Keypad key names (Windows + macOS)

| Commit | Change |
|---|---|
| _this commit_ | Name the keypad's arithmetic keys apart from the main row, as the Linux backend already does |

`gpui_windows`' `parse_immutable` maps `VK_MULTIPLY` / `VK_ADD` / `VK_SUBTRACT`
/ `VK_DIVIDE` / `VK_DECIMAL` to `"multiply"` / `"add"` / `"subtract"` /
`"divide"` / `"decimal"`, and `gpui_macos`' `keypad_key_name` does the same by
hardware key code (`kVK_ANSI_Keypad*`). Both used to fall through to a plain
`"*"` / `"+"` / `"-"` — `MapVirtualKeyW(MAPVK_VK_TO_CHAR)` on Windows,
`charactersIgnoringModifiers` on macOS — which made the two keys
indistinguishable to a keymap. Linux has named them all along
(`is_keypad_key()` in `gpui_linux`'s `platform.rs`), so this closes a
platform gap rather than inventing a convention.

`key_char` is untouched on both platforms: the keypad's `*` still types `"*"`
into a text field, it just no longer *matches* the main row's `*` in a keymap.
The keypad's digits keep the main row's names and its Enter stays `"enter"` —
both deliberate, both what Linux does.

Elane needs this: a file manager binds the numpad's `*` to "invert selection"
while `*` typed on the main row starts a wildcard search. Under the old naming
the binding swallowed the wildcard.

## 7. Windows text rendering (DirectWrite)

| Commit | Change |
|---|---|
| `3926b72b` | Apply embolden + blur to glow glyphs in DirectWrite |
| `1e745ba1` | Preserve weight/style when falling back to alternative fonts |

## 8. Window / hover / resize behavior

| Commit | Change |
|---|---|
| `df5caa72` | Add `suspend_hovers` to suppress hover states during drag/resize |
| `9436b547` | Suspend hovers during window resize |
| `5b2df28f` | (Wayland) defer surface state changes to `draw()` to fix 1px resize "wabbern" on Mutter |
| `71ea59bb` / `0f4c1776` | (Wayland/Windows) suppress "window not found" error on window close |
| `047e0ae0` | (Wayland) complete the above for the path GPUI itself closes a window on. `Window::remove_window` takes the entry out of `App`'s map and drops the `Rc<WaylandWindow>`; `WaylandWindow::drop` destroys the surface immediately but defers `close()` — which is what set `Callbacks::closed` — to a spawned task, leaving the window in the client's surface map in between. The pending `wl_callback` (one is essentially always in flight) is still routed there, and `frame()` was never guarded, so its `thermal_state` / `present` / `complete_frame` updates produced three `window not found` backtraces per close. Sets `closed` synchronously in `Drop` and guards `frame()` plus the remaining dispatch sites via `take_callback`. X11 already does this with its `destroyed` flag |
| `c94d338b` | Propagate `map_window` error instead of `unwrap` |
| `1a88f2c2` | (macOS) `WindowBackgroundAppearance::Blurred`: drop upstream's `NSVisualEffectView` path entirely and use the WindowServer blur (`CGSSetWindowBackgroundBlurRadius(80)`, upstream's pre-Monterey-only path) on all macOS versions. On macOS 26 the materials are unusable for a colorless backdrop: the reworked layer tree renders BLACK under upstream's `updateLayer` tint-stripping, and an untinted `UnderWindowBackground` material fogs the desktop almost completely (~4% transmission measured under Elane's Liquid-Glass chrome vs ~20% with the colorless CGS blur, 2026-07-27). The CGS API still works on macOS 26 (verified visually, same API iTerm2 uses) and leaves tinting to the app's translucent fills |

## 9. Image handling

| Commit | Change |
|---|---|
| `6c686147` | Respect animated WebP loop count instead of looping forever |

## 10. Text system performance

| Commit | Change |
|---|---|
| `93a1d339` | Cache font-resolution failures as `Arc<anyhow::Error>` — cache hits for a missing family no longer construct a fresh anyhow error (= backtrace capture when `RUST_BACKTRACE` is set) per text line per frame |

## 11. Layout engine (taffy)

| Commit | Change |
|---|---|
| (pending) | **taffy `=0.10.1` → `=0.12.1`.** Taffy 0.10/0.11 has a layout bug: inside a `Display::Block` container that is itself an auto-sized flex item (gpui's plain `div()` default), a flex-column's percent-width child collapses to width 0 whenever a sibling subtree contains flex items with a pixel `flex_basis`. Real-world symptom: Elane's embedded terminal (`h(300).w_full()` next to resizable panels with measured `flex_basis`) rendered 1 column wide. Fixed upstream in taffy 0.12. Regression test: `taffy::tests::percent_width_child_in_block_wrapped_flex_column` (a downgrade also fails to build: `style.rs` uses taffy 0.12's `AlignItems::START` keyword consts). **When re-merging upstream zed (still pins `=0.10.1`), keep the 0.12 pin.** |
| (pending) | `GPUI_LAYOUT_DEBUG=1` dumps every solved taffy tree (per-node style + computed layout) to stderr — replay against a standalone taffy crate to bisect engine-level layout bugs. See `TaffyLayoutEngine::compute_layout`. |

## 12. Zero-copy DMABuf video surfaces (Linux)

| Commit | Change |
|---|---|
| (pending) | **Linux zero-copy video presentation.** New `gpui::DmabufFrame`/`DmabufPlane`/`DmabufFormat` types (`gpui/src/dmabuf.rs`) + `SurfaceSource::Dmabuf` arm and a Linux `surface()`/`Window::paint_surface`, resurrecting the previously macOS-only surface primitive. wgpu device creation routes through wgpu-hal `open_with_callback` to enable `VK_EXT_image_drm_format_modifier` (`external_memory_fd`/`_dma_buf` auto-added), falling back to `request_device`; `wgpu_context.rs` queries the device's importable NV12 DRM modifiers (`DrmFormatModifierPropertiesListEXT`, 2-memory-plane + SAMPLED) exposed as `Window::supported_dmabuf_formats()` for Wayland-compositor-style format negotiation with producers. New `gpui_wgpu/src/dmabuf_texture.rs` imports a frame as ONE multiplanar `VkImage` (DRM-modifier tiling, explicit plane layouts, dedicated fd import via ash) wrapped as `wgpu::TextureFormat::NV12`; the previously stubbed `PrimitiveBatch::Surfaces` draw arm samples `TextureAspect::Plane0/Plane1` views through the existing `fs_surface` pipeline. The YCbCr→RGB matrix moved from a shader constant (BT.601 full — wrong for HD) into `SurfaceParams`, supplied per frame (BT.601 full/limited, BT.709/BT.2020 limited presets). New dep: `ash 0.38` (matching wgpu-hal) on Linux. Consumer: Elane's video preview (elane-media). |

## 12b. Zero-copy shared-D3D11-texture video surfaces (Windows)

| Commit | Change |
|---|---|
| `ff0a013d78` + `1619958422` | **Windows zero-copy video presentation** (counterpart of §12). New `gpui::D3d11Frame` type (`gpui/src/d3d11_surface.rs`: NT shared handle + keyed-mutex keys + owner keepalive) + `SurfaceSource::D3d11` arm, Windows `surface()`/`Window::paint_surface`, platform-neutral `Window::supports_video_surfaces()` (macOS true / Linux dmabuf check / Windows `PlatformWindow::supports_d3d11_surfaces`). `gpui_windows`: the previously stubbed `draw_surfaces` now opens producer textures via `ID3D11Device1::OpenSharedResource1` (cached by handle value, cleared on device loss), synchronizes through `IDXGIKeyedMutex` (AcquireSync via **vtable** — windows-rs folds WAIT_TIMEOUT into `Ok(())`; timeout = skip frame, never stall), and draws through a new `surface` HLSL module (unit quad sampling the BGRA texture, content-mask clipped; `SurfaceSprite` instance pipeline; module added to build.rs fxc list). Producer protocol: acquire 0-first-then-1, release to 1; consumers acquire/release 1 so paused frames stay re-acquirable across redraws. Consumer: Elane's video preview (elane-media `windows_mf.rs`). |

## 12c. Re-entrant window-message diagnostics (Windows)

| Commit | Change |
|---|---|
| (pending) | **`on_hit_test_window_control`: answer re-entrant hit tests from a cache instead of erroring.** WM_NCHITTEST arrives synchronously while the App RefCell may already be borrowed (input dispatch, draw — easy to hit with continuously-animating content like video). The cursor cannot have moved within the re-entrant call, so the last successful answer is exact; previously this logged "RefCell already borrowed" and degraded the hit test to `None`. Additionally the `request_frame` callback's updates now use `log_err_with_backtrace()` — when the frame message is delivered re-entrantly, the failure stack names whoever pumped the message queue. `ResultExt::log_err_with_backtrace` appends a `Backtrace::force_capture()` at the log site whenever the error carries no captured backtrace (anyhow only captures at *construction*, gated on `RUST_[LIB_]BACKTRACE`, and std caches that env lookup on first use — so env arming is fragile); sync-result log sites share the error's construction stack, so the fallback stack is the real one. This is what identified Elane's `IMFMediaEngine::Shutdown`-in-`Player::drop` as the message-queue pumper. |

## 13. Docs

| Commit | Change |
|---|---|
| `dd6bf383` | Doc comment for `Window::paint_glyph` |
| (in diff) | `ACCESSIBILITY_PLAN.md` (+34) |


## 14. What the 2026-09-01 upstream merge moved

Not new patches — the same ones, re-seated. Recorded because the next merge
will land on this shape, not the one the sections above describe.

| Was | Is |
|---|---|
| `gpui_macos/src/{metal_renderer,metal_atlas,shaders.metal}` | `gpui_apple/src/…` — upstream extracted the crate. The fork's own `metal_custom_shader.rs` moved with them (its only caller is `metal_renderer.rs`), and the `naga` dependency moved from `gpui_macos` to `gpui_apple`. |
| `PlatformWindow::completed_frame` | `PlatformWindow::schedule_frame`. Upstream removed the former with the demand-driven Wayland loop (#60690). The Wayland backend's fork patches sit on the new loop: the `closed` guard and the `pending_drawable_size` / `pending_viewport_dest` deferral are unchanged, `force_render_after_recovery` became upstream's `redraw_requested` (the field itself is gone; **X11 still has its own**), and the wlroots empty-commit workaround is dropped — `complete_frame`'s presentation state machine commits on the `RetryAfterPresent` path instead. |
| `WM_GPUI_NATIVE_DRAG = WM_USER + 9` | `WM_USER + 100`. Upstream's new `WM_GPUI_END_SESSION` took `+9`. Fork window messages start at `+100` from now on, so upstream can keep growing its block one at a time. |
| Two `DirectXRenderer::render_to_image` | One. Upstream grew its own — device-lost guard, `background_appearance`, and it goes through `render` rather than duplicating the batch loop — so the fork's copy is gone and upstream's is ungated instead, which is all the fork ever wanted from it. |

One thing to watch on the next merge: the throttled-configure early return in
`wayland/window.rs` (the 1px Mutter "wabbern" fix) now calls `request_redraw()`
before returning. Nothing ticks on its own any more, and `resize_throttle` is
cleared at the top of `frame()` — parking with the flag still set would skip
every further resizing configure and freeze the window for the rest of the drag.
---


## Platform coverage of the diff

- **`gpui` core**: `window.rs` (+310), `text_system*` (+210), `img.rs`, `scene.rs`, `platform.rs`, `style.rs`/`styled.rs`/`div.rs` (effects + glow plumbing)
- **Windows** (`gpui_windows`): drag, custom shaders, DirectWrite, keyboard, clipboard, events — the heaviest-patched backend
- **Linux** (`gpui_linux`): Wayland client/window/clipboard, X11 — drag-and-drop and resize fixes
- **wgpu** (`gpui_wgpu`): renderer + shaders + cosmic text — Linux shader/screenshot support
- **macOS** (`gpui_macos`): metal renderer, pasteboard, shaders, custom-shader runtime
- **Web** (`gpui_web`): custom shader support

---

## Full file-level diff stat (vs `ce48461e`)
```
 ACCESSIBILITY_PLAN.md                            |  49 ++++++++
 Cargo.lock                                       |   7 +-
 Cargo.toml                                       |   1 +
 FORK_CHANGES.md                                  | 275 +++++++++++++++++++++++++++++++++++++++++
 crates/gpui/Cargo.toml                           |   4 +-
 crates/gpui/src/app.rs                           |   8 ++
 crates/gpui/src/assets.rs                        |  23 ++++
 crates/gpui/src/d3d11_surface.rs                 |  64 ++++++++++
 crates/gpui/src/dmabuf.rs                        | 135 ++++++++++++++++++++
 crates/gpui/src/elements/div.rs                  |   8 ++
 crates/gpui/src/elements/img.rs                  |  82 +++++++++++--
 crates/gpui/src/elements/surface.rs              |  56 ++++++++-
 crates/gpui/src/elements/text.rs                 |  39 ++++--
 crates/gpui/src/gpui.rs                          |   8 ++
 crates/gpui/src/platform.rs                      |  89 +++++++++++++-
 crates/gpui/src/scene.rs                         | 159 ++++++++++++++++++++++++
 crates/gpui/src/style.rs                         |  18 +++
 crates/gpui/src/styled.rs                        |  12 +-
 crates/gpui/src/taffy.rs                         | 155 +++++++++++++++++++++++
 crates/gpui/src/text_system.rs                   | 132 ++++++++++++++++----
 crates/gpui/src/text_system/glow_mask.rs         | 182 +++++++++++++++++++++++++++
 crates/gpui/src/text_system/line.rs              | 115 ++++++++++++++++-
 crates/gpui/src/window.rs                        | 530 +++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++-----
 crates/gpui/src/window/a11y.rs                   |  18 ++-
 crates/gpui_apple/Cargo.toml                     |   5 +
 crates/gpui_apple/build.rs                       |   4 +
 crates/gpui_apple/src/gpui_apple.rs              |   5 +
 crates/gpui_apple/src/metal_custom_shader.rs     | 522 +++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++
 crates/gpui_apple/src/metal_renderer.rs          | 550 +++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++--
 crates/gpui_apple/src/shaders.metal              | 179 ++++++++++++++++++++++++++-
 crates/gpui_linux/Cargo.toml                     |   1 +
 crates/gpui_linux/src/linux/platform.rs          |  47 ++++++-
 crates/gpui_linux/src/linux/wayland.rs           |   2 +-
 crates/gpui_linux/src/linux/wayland/client.rs    | 231 +++++++++++++++++++++++++++++++++-
 crates/gpui_linux/src/linux/wayland/clipboard.rs |  78 ++++++++++--
 crates/gpui_linux/src/linux/wayland/window.rs    | 237 +++++++++++++++++++++++++++++++----
 crates/gpui_linux/src/linux/x11/client.rs        |  42 +++++--
 crates/gpui_linux/src/linux/x11/clipboard.rs     |  16 ++-
 crates/gpui_linux/src/linux/x11/window.rs        |  29 ++++-
 crates/gpui_macos/src/pasteboard.rs              |  39 +++++-
 crates/gpui_macos/src/text_system.rs             |  23 +++-
 crates/gpui_macos/src/window.rs                  | 162 ++++++++----------------
 crates/gpui_shared_string/Cargo.toml             |   1 +
 crates/gpui_util/Cargo.toml                      |   1 +
 crates/gpui_util/src/lib.rs                      |  22 +++-
 crates/gpui_web/src/window.rs                    |  13 +-
 crates/gpui_wgpu/Cargo.toml                      |   5 +
 crates/gpui_wgpu/src/cosmic_text_system.rs       |  69 ++++++++++-
 crates/gpui_wgpu/src/dmabuf_texture.rs           | 206 +++++++++++++++++++++++++++++++
 crates/gpui_wgpu/src/gpui_wgpu.rs                |   2 +
 crates/gpui_wgpu/src/shaders.wgsl                |  96 +++++++++++++--
 crates/gpui_wgpu/src/wgpu_context.rs             | 181 +++++++++++++++++++++++++++
 crates/gpui_wgpu/src/wgpu_renderer.rs            | 635 +++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++-
 crates/gpui_windows/Cargo.toml                   |   1 +
 crates/gpui_windows/build.rs                     |   1 +
 crates/gpui_windows/src/clipboard.rs             |  37 +++++-
 crates/gpui_windows/src/direct_write.rs          |  41 +++++--
 crates/gpui_windows/src/directx_custom_shader.rs | 420 ++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++
 crates/gpui_windows/src/directx_renderer.rs      | 270 ++++++++++++++++++++++++++++++++++++++--
 crates/gpui_windows/src/events.rs                |  36 ++++++
 crates/gpui_windows/src/gpui_windows.rs          |   2 +
 crates/gpui_windows/src/keyboard.rs              |  71 +++++------
 crates/gpui_windows/src/native_drag.rs           | 522 +++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++
 crates/gpui_windows/src/shaders.hlsl             |  37 ++++++
 crates/gpui_windows/src/window.rs                |  51 +++++++-
 crates/sum_tree/Cargo.toml                       |   2 -
 crates/sum_tree/src/cursor.rs                    |   2 +-
 crates/sum_tree/src/sum_tree.rs                  |   7 +-
 68 files changed, 6693 insertions(+), 379 deletions(-)
```

_Last generated: 2026-09-01 (commit `ce85100f4a` at branch HEAD)._
