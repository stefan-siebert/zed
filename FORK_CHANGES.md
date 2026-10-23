# GPUI Fork — Patches & Changes

**Fork:** `stefan-siebert/zed`, branch `gpui-mcp-patches-v2`
**Built from:** local checkout `../gpui-fork` (overrides the git dep via Cargo `[patch]` in Elane's `Cargo.toml`).
**Baseline:** upstream Zed merged at commit `5adae449` (merge commit `77cfef62`, 2026-10-21; previous: `933d8d93` / `3f66c9e1`, 2026-09-25).

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

## Which upstream commit to merge (decided 2026-10-22)

**Not Zed's `main` — the Zed commit gpui-component builds against.** Upstream
gpui-component (longbridge) does not follow Zed's `main`: it depends on
`gpui-pre` from crates.io, a weekly snapshot of Zed published by its
maintainers, and moves to the next one within days. Merging Zed's `main`
put this fork 125 Zed commits ahead of that (2026-10-21, `5adae449` against
`gpui-pre 0.3.9` = `zed@93d7456`): gpui-component then used APIs Zed had
already renamed (`ScrollWheelEvent` → `ScrollEvent`, #65805; 118 deprecation
warnings), and longbridge had nothing to fix — for them the rename did not
exist yet. On the same commit, a gpui-component merge compiles against
exactly the gpui its upstream compiled against.

Find the commit:

```bash
git -C ../gpui-component fetch upstream
VERSION=$(git -C ../gpui-component show upstream/main:Cargo.toml \
  | sed -n 's/^gpui = { package = "gpui-pre", version = "=\([^"]*\)".*/\1/p')
curl -s -A elane https://crates.io/api/v1/crates/gpui-pre/$VERSION \
  | python3 -c 'import sys,json; print(json.load(sys.stdin)["version"]["description"])'
# → "… (gpui-pre snapshot of zed@93d7456)": merge that commit, not upstream/main
```

Rules:

- **Both forks in one pass, gpui-fork first** (gpui-component tracks the
  gpui it is built with): gpui-fork merges `zed@<snapshot>`, gpui-component
  merges `upstream/main`, then Elane raises both floors in `siblings.toml`.
- **Never move backwards** to reach a snapshot: if the fork is already
  past it, wait until gpui-component's `gpui-pre` overtakes the fork.
- **An upstream fix that cannot wait** is cherry-picked onto the fork on its
  own, with its PR number kept in the subject, not pulled in by merging
  `main`.
- **A merge made on a Mac is not done until Linux and Windows compile** —
  the Mac builds neither `gpui_wgpu` nor `gpui_linux`, and the 2026-10-21
  merge broke Linux that way (§15, Linux follow-up). Windows: Elane's
  `cargo xtask check-windows` runs on the Mac too. Linux: only on a Linux
  machine (or WSL) — Elane's `cargo clippy --workspace --all-targets` there
  before Elane's floor in `siblings.toml` is raised; `cargo xtask linux-ci`
  does not run on macOS.

---

## Summary

**84 custom commits**, net **+7,217 / −475 lines across 74 files** vs the upstream merge point (`5adae449`, measured 2026-10-21 after the merge commit and its follow-ups).

Both figures come straight from the commands above; re-run them after every
upstream merge, and move the baseline commit with them. Counting against
`origin/main` (the fork mirror) instead of the merge base silently folds in
upstream commits that carry no PR number.

⚠️ These forks are load-bearing (see Elane `CLAUDE.md`). `cargo update` against upstream will break the shader, inspector, drag, and clipboard work.

---

## 0. Licensing and dependency pins

| Commit | Change |
|---|---|
| `9e2509d7` | **`sum_tree` without `ztracing`/`zlog`** — both are GPL-3.0 and would make every gpui consumer GPL (Elane's plan `2026-05-13-gpui-fork-gpl-isolation.md`). `tracing::instrument` instead; the test logger init is gone. Re-check on every merge: upstream keeps adding `ztracing` to crates gpui pulls in (from Elane: `cargo tree --workspace -i ztracing` and `-i zlog` must find no package). |
| `09fdc7c3` | `gpui_web`: `unicode-properties = "0.1.3"` instead of upstream's unexplained `=0.1.3` pin, which cannot coexist with rustybuzz 0.20 (typst, Elane's PDF plugin). |

## 1. MCP / UI Inspector APIs (original purpose of the fork)

| Commit | Change |
|---|---|
| `d8b7b8b3` | Public APIs for MCP UI inspection and event dispatch (dispatch wrappers, inspector entry points) |
| `541898a1` | Capture painted text content for the MCP inspector |
| `78866100` | Deduplicate inspector text content — assign to smallest container |
| `48d68301` | **`Window::set_a11y_force_active`.** Upstream builds the AccessKit tree only while assistive technology is attached, which leaves it unreadable to anything that wants to *check* accessibility rather than consume it — a test, the MCP inspector, a CI run. Adds `force_enabled` to the per-window `A11y` and ORs it into `sync_active_flag`, so `Application::new_inaccessible` still wins and the default is unchanged. Takes effect from the next frame: the flag for the frame being painted is latched before the first node is pushed, and the builder late to that stack would push and pop unevenly. |
| `d869efe7` | **`InspectorElementInfo::accesskit_node_id`.** The only exact join between the inspector and the accessibility tree. A node records the leaf of its element id and its source location, and four title-bar buttons of the same widget share both — matching on that pair picks one at random. gpui already derives a node’s AccessKit id from the whole `GlobalElementId`, so the inspector now reports that id and the two sides join by identity. Computed inside gpui and only read outside it, so nothing downstream depends on `DefaultHasher` being stable. |
| `1dd7db18` | **`App::request_inspector_ids`.** Upstream (zed #64309, merged here 2026-09) builds inspector element ids only while a window's own inspector is open, and every hitbox the MCP inspector reads hangs off those ids — so after the merge `ui_snapshot` & co. saw an empty tree in every window. The flag makes every window build them; `Window::inspector_enabled` ORs it in. gpui-component's MCP server sets it on the first request, so an app no tool talks to keeps upstream's saving. Test: `inspector_tests::requested_inspector_ids_reach_every_window`. |

## 2. Per-element backdrop blur (macOS)

| Commit | Change |
|---|---|
| `e23cb9bd` | **`BackdropBlur` primitive + Metal implementation.** GPUI had no backdrop-filter primitive at all: both shader paths (Metal's `quad_fragment` `effect_type` branches and the WGSL `CustomShader` contract) bind no texture, so neither can read what is already painted behind an element. Adds `BackdropBlur` to `Scene` (`bounds`, `content_mask`, `corner_radii`, `blur_radius`, `tint`), `Window::paint_backdrop_blur`, and `PlatformWindow::supports_backdrop_blur` (default `false`). `PrimitiveKind::BackdropBlur` sits **between `Shadow` and `Quad`** so a backdrop at the same draw order paints under the quad that requested it, while every existing kind keeps its relative order. macOS implements it: a dual-Kawase chain (Bjorge, ARM, SIGGRAPH 2015) on a half-resolution ping-pong pair, composited through `quad_sdf` for rounded corners. wgpu and DirectX get a no-op arm and keep reporting `false`. |

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
| `d479e826` | **Windows `shaders.hlsl` `Quad` gains `effect_type`/`effect_pad`/`effect_params`.** `a9093714` grew `gpui::Quad` by 24 bytes and left the HLSL struct as it was, so the StructuredBuffer's stride and the shader's disagreed: DXVK (Wine) reads with the shader's stride and every quad after the first came out shifted and miscoloured; native drivers mostly read with the buffer's, which is why the glitch showed only on some screens (a settings page of small theme cards). Unused on Windows, there for the stride. **Any field added to a `scene.rs` primitive goes into every platform's shader struct.** |
| `cd841350` | Declarative `text_glow()` style property |
| `ac6e965d` | Noise, vignette, and shimmer quad effects |
| `e3fdf01f` | Custom shader primitives (Phase 5) |
| `bb228f48` | Custom shader support for Windows and Web platforms |
| `c31d1826` | DirectX runtime for custom shaders (Windows) |
| `f4828a22` | Restore `CustomShaders` arm in macOS metal renderer match |
| `7b4ffb22` | Fix Point/Size field access in vignette & shimmer effects |
| `ec534b9e` | Custom-shader instances: clear per frame + 16-byte instance stride |
| `f462812b` | **Metal runtime for custom shaders (macOS).** The last backend without one: `register_custom_shader` returned `None` on macOS, so no `PrimitiveBatch::CustomShaders` was ever produced and the match arm was an empty `{}`. Elane's treemap cushions and its About-dialog backdrop therefore ran on the flat-fill fallback on the fork's own development platform. New `gpui_macos/src/metal_custom_shader.rs` mirrors `directx_custom_shader.rs`: the caller's `custom_effect` fragment is wrapped in the same WGSL module the other two backends use, translated to MSL with `naga` (new dependency, `wgsl-in` + `msl-out`), compiled through `newLibraryWithSource:`, and cached per `CustomShaderId`. Instances ride the existing per-frame instance buffer; a batch binds the whole array and passes `range.start` as the base instance, since Metal's `[[instance_id]]` counts from it. Blending is premultiplied, matching wgpu/DirectX rather than gpui's own straight-alpha pipelines. Three things to preserve across merges: naga demands a `sizes_buffer` for *any* runtime-sized array whatever the bounds-check policy says, so `_mslBufferSizes` is bound at `buffer(3)`; the quad is a four-vertex triangle strip derived from `[[vertex_id]]`, so this is the one pipeline here that binds no vertex buffer; and the module carries three headless pixel tests, one of which fails if the base instance is dropped. |
| `0f18336c` | **Text glow on macOS.** `gpui_macos`'s CoreGraphics rasterizer ignored `RenderGlyphParams::embolden`, so glow glyphs rasterized as plain sharp glyphs painted underneath the foreground pass — fully occluded, i.e. `text_glow()` silently did nothing on macOS (Elane titlebar wordmark). The mask-space glow post-processing (`glow_padding_pixels` / `embolden_alpha_mask` / `blur_alpha_mask`), previously duplicated across `gpui_windows/direct_write.rs` and `gpui_wgpu/cosmic_text_system.rs`, moved to shared `gpui/src/text_system/glow_mask.rs` (+ unit tests); both backends now import it and `gpui_macos/text_system.rs` applies it (padded `raster_bounds`, dilate + blur on the CG alpha mask, emoji path skipped). Also: `gpui`'s dev-dependency on `gpui_platform` now enables `runtime_shaders` so `cargo test -p gpui` builds on macOS without the Xcode Metal Toolchain component. |

New/large files: `gpui_windows/src/directx_custom_shader.rs` (+419), `gpui_macos/src/metal_custom_shader.rs` (+500), `gpui_wgpu/src/wgpu_renderer.rs` (+464), `shaders.wgsl`, `shaders.metal`.

Since the 2026-09-25 merge the wgpu side sits on upstream's `WgpuRendererCore`
(shared with the headless renderer): compiled custom-shader pipelines live in the
core, the WGSL sources on `WgpuRenderer` (`custom_shaders`), so `recover()` after
a lost device recompiles them for the new one. `render_to_image` renders through
`core.render_frame` into an offscreen texture and reads it back (`read_texture`).

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

## 6a. Auto-repeat reaches keystroke interceptors

| Commit | Change |
|---|---|
| `7e5be29d` | **`KeystrokeEvent::is_held`.** `KeyDownEvent::is_held` is set by every platform but was dropped before `intercept_keystrokes` / `observe_keystrokes`, so an interceptor could not tell a held key's auto-repeat from fresh presses. Elane's F-key prefix (an interceptor that arms on Esc and flips switches on the arrows) flickered on a held key. Test: `key_dispatch::test_interceptors_see_auto_repeat`. |
| `1a9a042ff7` | **`Window::dispatch_keystroke_repeat`.** The same key-down as `dispatch_keystroke`, with `is_held: true` — what a tool simulating a held key sends per repeat (gpui-mcp's `send_key` with `repeat`, answered in gpui-component's `mcp.rs`). Test: `key_dispatch::test_dispatch_keystroke_repeat_is_held`. |

## 6b. Keypad key names (Windows + macOS)

| Commit | Change |
|---|---|
| `0d75da42` | Name the keypad's arithmetic keys apart from the main row, as the Linux backend already does |

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
| `d479e826` | **Font lookup falls back to `FindFamilyName` → `GetFirstMatchingFont`** when `IDWriteFontSet::GetMatchingFonts` fails — Wine 11's answers `E_NOTIMPL`, and without the fallback no font resolved at all. Windows keeps the font-set path. |

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

## 8a. Quit requests the platform makes (macOS)

| Commit | Change |
|---|---|
| `6efe33cb` | **`App::on_should_quit(handler)`**, answered by `applicationShouldTerminate:`. macOS ends an app on its own on the Dock's Quit, at logout and at shutdown; upstream does not implement the delegate method, so AppKit's default `NSTerminateNow` applied and an app could not refuse. `Platform::on_should_quit` (default: ignore — Windows/Linux have no such question) registers the callback; `gpui_macos` returns `NSTerminateCancel` when it says no. One handler returning `false` keeps the app running. `App::quit` sets `quit_requested` first: on macOS it ends in the same `terminate:`, and the quit the app asked for itself must not be refused. A borrowed `AppCell` when AppKit asks lets the quit through, as before. Test: `app::test::should_quit_asks_the_handlers_until_the_app_quits`. Elane refuses while file operations run and asks as on Cmd+Q (`MainWindow::allow_platform_quit`). |

## 8b. Window-control insets (iPadOS)

| Commit | Change |
|---|---|
| `8271d403` | **`WindowInsets::window_controls`** (`WindowControlsInsets { beside, below }`). A windowed iPadOS 26+ app shows close/minimize/full-screen over its top-leading corner, and `safeAreaInsets` does not cover them, so content drew underneath. `gpui_ios` now asks `edgeInsetsForLayoutRegion:` of `safeAreaLayoutRegionWithCornerAdaptation:` once per axis on every layout and reports what each adds to the safe area: `beside` (horizontal: content on the controls' line starts after them), `below` (vertical: content starts under them). The app picks per region; `effective()` leaves both out. Zero before iOS 26 (no `UIViewLayoutRegion` class — guarded by `respondsToSelector`), full screen, on an iPhone and on every other platform. Measured 2026-10-22, iPad Pro 11″ simulator, iPadOS 27.0: `beside.left` 66 pt (and `beside.right` 9.5), `below.top` 43 pt, on top of a 32 pt safe area; the very first frame still reports zero, the first layout pass corrects it. Needs objc2-ui-kit's `UIViewLayoutRegion` feature. |

## 8c. Hardware keyboard (iOS)

| Commit | Change |
|---|---|
| `c0799e8c2b` | **Hardware-keyboard presses become `KeyDown`/`KeyUp`/`ModifiersChanged`** (`gpui_ios/src/ios/keyboard.rs`). Upstream's backend had no `pressesBegan` at all, so a keymap never saw a key; the closed upstream attempt #63547 passed on only Ctrl/Cmd chords and navigation keys, which leaves out what a keyboard-driven app binds to bare keys. Now the Metal view hands every press to GPUI, and the text input view — first responder while an input has the soft keyboard — does too, passing a key on to UIKit's text system (`super`) only when GPUI left it unhandled: bindings first, typing after, as on macOS; without that a backspace would delete twice. While the text view is first responder the Metal view ignores presses, which then reach it only through that `super`. Names follow `gpui_macos` (by HID usage for keys that type nothing; keypad arithmetic as `divide`/`multiply`/`subtract`/`add`/`decimal`; shift with a non-letter names the shifted character, `shift-1` → `!`), the rest from `charactersIgnoringModifiers`. **UIKit never repeats a held key** (measured, iPadOS 27.0 simulator: ↓ held 2 s = one `began`, one `ended`), so held keys repeat here as `is_held` key-downs after 500 ms, every 33 ms — except a key that went to the text system, which keeps UIKit's own behaviour (repeat or the accent menu). Modifier presses (HID 0xE0–0xE7) are tracked per key, so releasing one Shift while the other is held keeps Shift. `capslock()` reports `AlphaShift`. **`keystroke.rs`**: Apple keyboard conventions apply on iOS too — `secondary` is Cmd, `unparse` writes `cmd-` (it wrote nothing for the platform modifier on iOS, so `cmd-c` read back as `c`), the display uses ⌘⌥⇧ symbols. Not changed: `KeyContext` still reports `os = unknown` on iOS. Not verified on a device: UIKit's repeat/accent behaviour for typed text. |

## 8d. File picker (iOS)

| Commit | Change |
|---|---|
| `6d4cd2d06e` | **`prompt_for_paths` through `UIDocumentPickerViewController`** (`gpui_ios/src/ios/document_picker.rs`; upstream answers an error). `initForOpeningContentTypes:asCopy: false`, so the app gets the original files and folders, not inbox copies: `directories` → `public.folder`, `files` → `public.item` (both when both are asked), `multiple` → `allowsMultipleSelection`; `prompt` has no counterpart. Presented from a foreground task, never inside a GPUI update, on the topmost presented view controller. Cancel answers `Ok(None)`; a second prompt while one is open answers the first with `Ok(None)`. UIKit holds the delegate weakly, so the platform keeps the last one. **Security scope:** GPUI returns paths, not URLs, so every picked URL gets `startAccessingSecurityScopedResource` here and keeps it for the process's lifetime; access after a relaunch needs a bookmark kept by the app. Measured in the iPad simulator (iPadOS 27.0): a folder under "On My iPad" comes back as its path, lists, and Cancel answers `Ok(None)`; files are greyed out in folder mode. **The simulator does not enforce the sandbox** — a fresh process read the same foreign folder without any picker — so whether the scope is needed and sufficient is not verified there; that needs a device. Not done: `prompt_for_new_path` (still an error; the picker can only export existing files), bookmarks. New dependency `objc2-uniform-type-identifiers` (`UTType`). |

## 9. Image handling

| Commit | Change |
|---|---|
| `6c686147` | Respect animated WebP loop count instead of looping forever |

## 10. Text system performance

| Commit | Change |
|---|---|
| `93a1d339` | Cache font-resolution failures as `Arc<anyhow::Error>` — cache hits for a missing family no longer construct a fresh anyhow error (= backtrace capture when `RUST_BACKTRACE` is set) per text line per frame |
| `7e751f5d` | `WhiteSpace::Nowrap` text collapses line breaks (`\n`, `\r`, NEL, U+2028/2029) into spaces, byte for byte, as CSS `nowrap` does — `shape_text` otherwise broke a single-line label at a `\n` in a file name. Tabs and `Normal` text untouched |

## 11. Layout engine (taffy)

| Commit | Change |
|---|---|
| `1ff7083f` | **taffy `=0.10.1` → `=0.12.1`.** Taffy 0.10/0.11 has a layout bug: inside a `Display::Block` container that is itself an auto-sized flex item (gpui's plain `div()` default), a flex-column's percent-width child collapses to width 0 whenever a sibling subtree contains flex items with a pixel `flex_basis`. Real-world symptom: Elane's embedded terminal (`h(300).w_full()` next to resizable panels with measured `flex_basis`) rendered 1 column wide. Fixed upstream in taffy 0.12. Regression test: `taffy::tests::percent_width_child_in_block_wrapped_flex_column` (a downgrade also fails to build: `style.rs` uses taffy 0.12's `AlignItems::START` keyword consts). **When re-merging upstream zed (still pins `=0.10.1`), keep the 0.12 pin.** |
| `1ff7083f` | `GPUI_LAYOUT_DEBUG=1` dumps every solved taffy tree (per-node style + computed layout) to stderr — replay against a standalone taffy crate to bisect engine-level layout bugs. See `TaffyLayoutEngine::compute_layout`. |

## 12. Zero-copy DMABuf video surfaces (Linux)

| Commit | Change |
|---|---|
| `463b39d6` | **Linux zero-copy video presentation.** New `gpui::DmabufFrame`/`DmabufPlane`/`DmabufFormat` types (`gpui/src/dmabuf.rs`) + `SurfaceSource::Dmabuf` arm and a Linux `surface()`/`Window::paint_surface`, resurrecting the previously macOS-only surface primitive. wgpu device creation routes through wgpu-hal `open_with_callback` to enable `VK_EXT_image_drm_format_modifier` (`external_memory_fd`/`_dma_buf` auto-added), falling back to `request_device`; `wgpu_context.rs` queries the device's importable NV12 DRM modifiers (`DrmFormatModifierPropertiesListEXT`, 2-memory-plane + SAMPLED) exposed as `Window::supported_dmabuf_formats()` for Wayland-compositor-style format negotiation with producers. New `gpui_wgpu/src/dmabuf_texture.rs` imports a frame as ONE multiplanar `VkImage` (DRM-modifier tiling, explicit plane layouts, dedicated fd import via ash) wrapped as `wgpu::TextureFormat::NV12`; the previously stubbed `PrimitiveBatch::Surfaces` draw arm samples `TextureAspect::Plane0/Plane1` views through the existing `fs_surface` pipeline. The YCbCr→RGB matrix moved from a shader constant (BT.601 full — wrong for HD) into `SurfaceParams`, supplied per frame (BT.601 full/limited, BT.709/BT.2020 limited presets). New dep: `ash 0.38` (matching wgpu-hal) on Linux. Consumer: Elane's video preview (elane-media). |

## 12b. Zero-copy shared-D3D11-texture video surfaces (Windows)

| Commit | Change |
|---|---|
| `ff0a013d78` + `1619958422` | **Windows zero-copy video presentation** (counterpart of §12). New `gpui::D3d11Frame` type (`gpui/src/d3d11_surface.rs`: NT shared handle + keyed-mutex keys + owner keepalive) + `SurfaceSource::D3d11` arm, Windows `surface()`/`Window::paint_surface`, platform-neutral `Window::supports_video_surfaces()` (macOS true / Linux dmabuf check / Windows `PlatformWindow::supports_d3d11_surfaces`). `gpui_windows`: the previously stubbed `draw_surfaces` now opens producer textures via `ID3D11Device1::OpenSharedResource1` (cached by handle value, cleared on device loss), synchronizes through `IDXGIKeyedMutex` (AcquireSync via **vtable** — windows-rs folds WAIT_TIMEOUT into `Ok(())`; timeout = skip frame, never stall), and draws through a new `surface` HLSL module (unit quad sampling the BGRA texture, content-mask clipped; `SurfaceSprite` instance pipeline; module added to build.rs fxc list). Producer protocol: acquire 0-first-then-1, release to 1; consumers acquire/release 1 so paused frames stay re-acquirable across redraws. Consumer: Elane's video preview (elane-media `windows_mf.rs`). |

## 12c. Re-entrant window-message diagnostics (Windows)

| Commit | Change |
|---|---|
| `7e069b19` | **`on_hit_test_window_control`: answer re-entrant hit tests from a cache instead of erroring.** WM_NCHITTEST arrives synchronously while the App RefCell may already be borrowed (input dispatch, draw — easy to hit with continuously-animating content like video). The cursor cannot have moved within the re-entrant call, so the last successful answer is exact; previously this logged "RefCell already borrowed" and degraded the hit test to `None`. Additionally the `request_frame` callback's updates now use `log_err_with_backtrace()` — when the frame message is delivered re-entrantly, the failure stack names whoever pumped the message queue. `ResultExt::log_err_with_backtrace` appends a `Backtrace::force_capture()` at the log site whenever the error carries no captured backtrace (anyhow only captures at *construction*, gated on `RUST_[LIB_]BACKTRACE`, and std caches that env lookup on first use — so env arming is fragile); sync-result log sites share the error's construction stack, so the fallback stack is the real one. This is what identified Elane's `IMFMediaEngine::Shutdown`-in-`Player::drop` as the message-queue pumper. |

## 12d. Running under Wine (Windows)

| Commit | Change |
|---|---|
| `ab9f82ab` | **Under Wine (`ntdll!wine_get_version`): no DirectComposition, no frame.** Wine's `dcomp` is three `E_NOTIMPL` stubs and DXVK's `CreateSwapChainForComposition` is unimplemented (Proton and GE-Proton the same, checked 2026-10-19), so the window renders without it unasked. Nothing hides the resize frame `DefWindowProc` leaves left/right/bottom there — it was drawn white — so the whole window is client area and the resize edges are hit-tested inside it. |
| `ab9f82ab` | **`WM_ERASEBKGND` is not erased.** Upstream too: the class brush is black and erasing showed as a black flash on every resize wherever the window has a GDI surface — without DirectComposition (Wine, `GPUI_DISABLE_DIRECT_COMPOSITION=1`). With it the window has none (`WS_EX_NOREDIRECTIONBITMAP`) and nothing changes. |
| `d479e826` | **Direct Manipulation is optional**: a window whose `DirectManipulationHandler` cannot be created (Wine has none) logs it and goes on; touchpad gestures then arrive as wheel messages. With the rows above and the font/quad fixes, a debug build runs under Wine 11 + DXVK (Elane, 2026-10-19). |

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
## 15. What the 2026-10-21 upstream merge moved

368 upstream commits (`933d8d93..5adae449`). Merge commit `77cfef62`; the full resolution list is in
its message. The ones the next merge lands on:

| Was | Is |
|---|---|
| `gpui_macos/src/text_system.rs` | `gpui_apple/src/text_system.rs` (shared with iOS, #65058). The glow mask post-processing (`0f18336c`) moved with it; `Hsla`/`Rgba` are imported macOS-only there. |
| `SurfaceSource::Surface(core_video::CVPixelBuffer)` | `SurfaceSource::Surface(CFRetained<objc2_core_video::CVPixelBuffer>)` (objc2, also iOS). The fork's `Dmabuf` / `D3d11` variants sit next to it with the cfg widened to linux/windows. **Consumers change too**: Elane wraps elane-media's raw `CVPixelBufferRef` with `CFRetained::retain`. |
| `gpui_apple` on `cocoa`/`objc` | objc2 only. Nothing of the fork used the old crates any more; `naga` (custom shaders) and `collections` stay fork deps. |
| `layer.set_framebuffer_only(false)` unconditional | Upstream now gates it on `test-support` + debug. **Kept unconditional** — backdrop blur (§2) samples the drawable. |
| `TextLayout::paint(text)` | Upstream split it into `paint_lines(pass)` with public `paint_background` / `paint_foreground`. Glow paints in the foreground pass. `record_painted_text` (§1) moved into `paint(text)`, the only path that knows the text — the split passes record nothing for the inspector. |
| `KeystrokeEvent { is_held }` | Now next to upstream's `input_preference` (#64859); observer/interceptor calls pass both. |
| macOS `Blurred` | Upstream fixed it for macOS 27 with an `UnderWindowBackground` `NSVisualEffectView` (#65361). **Not taken**: the fork measured exactly that material fogging the desktop on macOS 26 (`1a88f2c2`); the WindowServer blur stays, `BlurredView` stays unregistered. Re-measure on macOS 27 before ever switching. |
| Wayland `state.renderer` | An `Option` upstream now. `draw()` returns before the fork's deferred drawable size / viewport are applied when there is no renderer, so they wait for the first one. `frame()` grew `(signal_at, signal_source)`; the fork's `closed` guard stays at its top. |
| `ScrollWheelEvent` / `PlatformInput::ScrollWheel` / `on_scroll_wheel` | `ScrollEvent` (new field `source: ScrollSource`) / `PlatformInput::Scroll` / `on_scroll` (#65805). The old type names stay as deprecated aliases; the enum variant does **not** — code that constructs input events (gpui-component's `mcp.rs`, tests) must say `Scroll` and set `source`. |

Verified 2026-10-21 on macOS (`runtime_shaders`, no Metal toolchain on the Mac mini): `cargo test -p gpui --lib`
392 passed, `cargo test -p gpui_apple --features runtime_shaders` 12 passed (custom-shader pixel tests included).
Windows and Linux backends are compiled by Elane's `cargo xtask check-windows` / the Linux CI lane, not here.

**Linux follow-up (2026-10-22).** The merge did not compile on Linux — the Mac never builds `gpui_wgpu`
or `gpui_linux` — because the fork's own code there had not moved with upstream:

| Was | Is |
|---|---|
| `DeviceDescriptor { … }` (the dmabuf device from `create_device_from_hal`) | Needs `default_queue: wgpu::QueueDescriptor::default()`, as upstream's own descriptor. |
| `create_texture_from_hal(hal, &desc)` (dmabuf import) | `(hal, &desc, initial_state, cleared)` (wgpu #9496). `TextureUses::UNINITIALIZED` — what wgpu hard-coded before, the image is created in `ImageLayout::UNDEFINED` — and `cleared: true`: the decoder wrote the frame, the texture has no clear mode. |
| `get_mapped_range{,_mut}()` → view | → `Result<_, MapRangeError>`. `read_texture` propagates it; `draw_surfaces` logs and skips that frame, as for a failed import. |
| `state.renderer.register_custom_shader` / `render_to_image` / `supports_dmabuf_surfaces` / `supported_dmabuf_formats` (Wayland and X11) | Through the `Option`: it is `None` only after the window closed — no shader, an error, no dmabuf. |

Verified on Linux (Fedora, Wayland, `cage` headless): Elane's workspace clippy clean, 4646 tests pass, an H.264
preview plays and the inspector's screenshot (`render_to_image`) works.

---


## Platform coverage of the diff

- **`gpui` core**: `window.rs` (+310), `text_system*` (+210), `img.rs`, `scene.rs`, `platform.rs`, `style.rs`/`styled.rs`/`div.rs` (effects + glow plumbing)
- **Windows** (`gpui_windows`): drag, custom shaders, DirectWrite, keyboard, clipboard, events — the heaviest-patched backend
- **Linux** (`gpui_linux`): Wayland client/window/clipboard, X11 — drag-and-drop and resize fixes
- **wgpu** (`gpui_wgpu`): renderer + shaders + cosmic text — Linux shader/screenshot support
- **macOS** (`gpui_macos`): metal renderer, pasteboard, shaders, custom-shader runtime
- **Web** (`gpui_web`): custom shader support

---

## Full file-level diff stat (vs `5adae449`)
```
 ACCESSIBILITY_PLAN.md                            |  49 +++++
 Cargo.lock                                       |  66 +++++-
 Cargo.toml                                       |   1 +
 FORK_CHANGES.md                                  | 368 ++++++++++++++++++++++++++++++++
 crates/gpui/Cargo.toml                           |   4 +-
 crates/gpui/src/app.rs                           |  34 +++
 crates/gpui/src/assets.rs                        |  23 ++
 crates/gpui/src/d3d11_surface.rs                 |  64 ++++++
 crates/gpui/src/debug_overlay.rs                 |   4 +
 crates/gpui/src/dmabuf.rs                        | 135 ++++++++++++
 crates/gpui/src/element.rs                       |   2 +-
 crates/gpui/src/elements/div.rs                  |   8 +
 crates/gpui/src/elements/img.rs                  |  82 +++++++-
 crates/gpui/src/elements/surface.rs              |  48 ++++-
 crates/gpui/src/elements/text.rs                 | 149 ++++++++++++-
 crates/gpui/src/gpui.rs                          |   8 +
 crates/gpui/src/key_dispatch.rs                  |  52 +++++
 crates/gpui/src/platform.rs                      |  89 +++++++-
 crates/gpui/src/scene.rs                         | 163 +++++++++++++++
 crates/gpui/src/style.rs                         |  18 ++
 crates/gpui/src/styled.rs                        |  12 +-
 crates/gpui/src/taffy.rs                         | 155 ++++++++++++++
 crates/gpui/src/text_system.rs                   | 126 +++++++++--
 crates/gpui/src/text_system/glow_mask.rs         | 182 ++++++++++++++++
 crates/gpui/src/text_system/line.rs              | 120 ++++++++++-
 crates/gpui/src/window.rs                        | 602 ++++++++++++++++++++++++++++++++++++++++++++++++++---
 crates/gpui/src/window/a11y.rs                   |  18 +-
 crates/gpui_apple/Cargo.toml                     |   7 +
 crates/gpui_apple/build.rs                       |   4 +
 crates/gpui_apple/src/gpui_apple.rs              |   5 +
 crates/gpui_apple/src/metal_custom_shader.rs     | 522 ++++++++++++++++++++++++++++++++++++++++++++++
 crates/gpui_apple/src/metal_renderer.rs          | 552 +++++++++++++++++++++++++++++++++++++++++++++++-
 crates/gpui_apple/src/shaders.metal              | 179 +++++++++++++++-
 crates/gpui_apple/src/text_system.rs             |  27 ++-
 crates/gpui_linux/Cargo.toml                     |   1 +
 crates/gpui_linux/src/linux/platform.rs          |  47 ++++-
 crates/gpui_linux/src/linux/wayland.rs           |   2 +-
 crates/gpui_linux/src/linux/wayland/client.rs    | 231 +++++++++++++++++++-
 crates/gpui_linux/src/linux/wayland/clipboard.rs |  78 ++++++-
 crates/gpui_linux/src/linux/wayland/window.rs    | 233 ++++++++++++++++++---
 crates/gpui_linux/src/linux/x11/client.rs        |  42 +++-
 crates/gpui_linux/src/linux/x11/clipboard.rs     |  16 +-
 crates/gpui_linux/src/linux/x11/window.rs        |  30 ++-
 crates/gpui_macos/src/events.rs                  |  31 +++
 crates/gpui_macos/src/pasteboard.rs              |  39 +++-
 crates/gpui_macos/src/window.rs                  | 167 +++++----------
 crates/gpui_util/src/lib.rs                      |  18 +-
 crates/gpui_web/Cargo.toml                       |   5 +-
 crates/gpui_web/src/window.rs                    |  12 +-
 crates/gpui_wgpu/Cargo.toml                      |  10 +-
 crates/gpui_wgpu/src/cosmic_text_system.rs       |  78 ++++++-
 crates/gpui_wgpu/src/dmabuf_texture.rs           | 206 ++++++++++++++++++
 crates/gpui_wgpu/src/gpui_wgpu.rs                |   2 +
 crates/gpui_wgpu/src/shaders.wgsl                |  96 ++++++++-
 crates/gpui_wgpu/src/shaders_webgl.wgsl          |  82 +++-----
 crates/gpui_wgpu/src/wgpu_context.rs             | 181 ++++++++++++++++
 crates/gpui_wgpu/src/wgpu_renderer.rs            | 520 ++++++++++++++++++++++++++++++++++++++++++++-
 crates/gpui_windows/Cargo.toml                   |   1 +
 crates/gpui_windows/build.rs                     |   1 +
 crates/gpui_windows/src/clipboard.rs             |  37 +++-
 crates/gpui_windows/src/direct_write.rs          | 110 +++++++---
 crates/gpui_windows/src/directx_custom_shader.rs | 420 +++++++++++++++++++++++++++++++++++++
 crates/gpui_windows/src/directx_renderer.rs      | 270 ++++++++++++++++++++++--
 crates/gpui_windows/src/events.rs                | 120 ++++++++++-
 crates/gpui_windows/src/gpui_windows.rs          |   2 +
 crates/gpui_windows/src/keyboard.rs              |  71 +++----
 crates/gpui_windows/src/native_drag.rs           | 522 ++++++++++++++++++++++++++++++++++++++++++++++
 crates/gpui_windows/src/platform.rs              |   5 +-
 crates/gpui_windows/src/shaders.hlsl             |  44 ++++
 crates/gpui_windows/src/util.rs                  |  14 ++
 crates/gpui_windows/src/window.rs                |  58 +++++-
 crates/sum_tree/Cargo.toml                       |   3 +-
 crates/sum_tree/src/cursor.rs                    |   2 +-
 crates/sum_tree/src/sum_tree.rs                  |   7 +-
 74 files changed, 7217 insertions(+), 475 deletions(-)
```

_Last generated: 2026-10-21 (after merge commit `77cfef62`)._
