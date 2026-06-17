# GPUI Fork — Patches & Changes

**Fork:** `stefan-siebert/zed`, branch `gpui-mcp-patches-v2`
**Built from:** local checkout `../gpui-fork` (overrides the git dep via Cargo `[patch]` in Elane's `Cargo.toml`).
**Baseline:** upstream Zed merged at commit `832c17e8` (`Merge remote-tracking branch 'upstream/main'`).

This file inventories the **custom commits on top of upstream**. Everything else on the branch is upstream PRs pulled in by merges.

## How to regenerate this list

```bash
cd ../gpui-fork
# Custom (non-PR-numbered) commits vs the fork mirror:
git log --oneline --no-merges origin/main..gpui-mcp-patches-v2 | grep -vE '\(#[0-9]+\)$'
# Net diff of all custom patches vs the upstream merge point:
git diff --stat 832c17e8..gpui-mcp-patches-v2
```

Upstream PRs are tagged `(#NNNNN)`; custom patches use conventional-commit style without a PR number.

---

## Summary

**30 custom commits**, net **+3,537 / −186 lines across 43 files** vs the upstream merge point.

⚠️ These forks are load-bearing (see Elane `CLAUDE.md`). `cargo update` against upstream will break the shader, inspector, drag, and clipboard work.

---

## 1. MCP / UI Inspector APIs (original purpose of the fork)

| Commit | Change |
|---|---|
| `d8b7b8b3` | Public APIs for MCP UI inspection and event dispatch (dispatch wrappers, inspector entry points) |
| `541898a1` | Capture painted text content for the MCP inspector |
| `78866100` | Deduplicate inspector text content — assign to smallest container |

## 2. Custom shaders / visual effects (largest feature area)

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

New/large files: `gpui_windows/src/directx_custom_shader.rs` (+419), `gpui_wgpu/src/wgpu_renderer.rs` (+464), `shaders.wgsl`, `shaders.metal`.

## 3. Screenshot / render-to-image (inspector & screenshots)

| Commit | Change |
|---|---|
| `a820fc97` | `render_to_image` for Windows DirectX renderer |
| `d8d06683` | `render_to_image` for wgpu renderer (Linux screenshot support) |
| `c4724161` | Ungate `render_to_image` on macOS + clean stale `NSBeep` unsafe |

## 4. Native drag-and-drop

| Commit | Change |
|---|---|
| `c9833333` | Native drag-and-drop for Linux/Wayland |
| `0d9e0d0c` | Windows native drag image DPI scaling + cursor offset sync |
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
| `c94d338b` | Propagate `map_window` error instead of `unwrap` |

## 9. Image handling

| Commit | Change |
|---|---|
| `6c686147` | Respect animated WebP loop count instead of looping forever |

## 10. Docs

| Commit | Change |
|---|---|
| `dd6bf383` | Doc comment for `Window::paint_glyph` |
| (in diff) | `ACCESSIBILITY_PLAN.md` (+34) |

---

## Platform coverage of the diff

- **`gpui` core**: `window.rs` (+310), `text_system*` (+210), `img.rs`, `scene.rs`, `platform.rs`, `style.rs`/`styled.rs`/`div.rs` (effects + glow plumbing)
- **Windows** (`gpui_windows`): drag, custom shaders, DirectWrite, keyboard, clipboard, events — the heaviest-patched backend
- **Linux** (`gpui_linux`): Wayland client/window/clipboard, X11 — drag-and-drop and resize fixes
- **wgpu** (`gpui_wgpu`): renderer + shaders + cosmic text — Linux shader/screenshot support
- **macOS** (`gpui_macos`): metal renderer, pasteboard, shaders
- **Web** (`gpui_web`): custom shader support

---

## Full file-level diff stat (vs `832c17e8`)

```
 ACCESSIBILITY_PLAN.md                            |  34 +++++
 Cargo.lock                                       |   2 +
 Cargo.toml                                       |   2 +-
 crates/gpui/src/app.rs                           |   4 +
 crates/gpui/src/assets.rs                        |  23 +++
 crates/gpui/src/elements/div.rs                  |   8 +
 crates/gpui/src/elements/img.rs                  |  80 ++++++++--
 crates/gpui/src/elements/text.rs                 |  39 +++--
 crates/gpui/src/platform.rs                      |  66 ++++++--
 crates/gpui/src/scene.rs                         |  77 ++++++++++
 crates/gpui/src/style.rs                         |  15 ++
 crates/gpui/src/styled.rs                        |  14 +-
 crates/gpui/src/text_system.rs                   |  98 +++++++++++-
 crates/gpui/src/text_system/line.rs              | 112 +++++++++++++-
 crates/gpui/src/window.rs                        | 310 +++++++++++++++++++++++++++++++++++--
 crates/gpui_linux/Cargo.toml                     |   2 +-
 crates/gpui_linux/src/linux/platform.rs          |  47 +++++-
 crates/gpui_linux/src/linux/wayland.rs           |   2 +-
 crates/gpui_linux/src/linux/wayland/client.rs    | 252 +++++++++++++++++++++++++++++-
 crates/gpui_linux/src/linux/wayland/clipboard.rs |  79 ++++++++--
 crates/gpui_linux/src/linux/wayland/window.rs    | 156 ++++++++++++++++---
 crates/gpui_linux/src/linux/x11/client.rs        |  42 +++--
 crates/gpui_linux/src/linux/x11/clipboard.rs     |  16 +-
 crates/gpui_linux/src/linux/x11/window.rs        |  20 ++-
 crates/gpui_macos/src/metal_renderer.rs          |   3 +-
 crates/gpui_macos/src/pasteboard.rs              |  39 ++++-
 crates/gpui_macos/src/shaders.metal              |  36 ++++-
 crates/gpui_macos/src/window.rs                  |   6 +-
 crates/gpui_web/src/window.rs                    |  20 ++-
 crates/gpui_wgpu/Cargo.toml                      |   1 +
 crates/gpui_wgpu/src/cosmic_text_system.rs       | 117 +++++++++++++-
 crates/gpui_wgpu/src/shaders.wgsl                |  84 +++++++++-
 crates/gpui_wgpu/src/wgpu_renderer.rs            | 464 +++++++++++++++++++++++++++++++++++++++++++++++++++++++-
 crates/gpui_windows/Cargo.toml                   |   1 +
 crates/gpui_windows/src/clipboard.rs             |  37 ++++-
 crates/gpui_windows/src/direct_write.rs          | 141 +++++++++++++++--
 crates/gpui_windows/src/directx_custom_shader.rs | 419 ++++++++++++++++++++++++++++++++++++++++++++++++++
 crates/gpui_windows/src/directx_renderer.rs      | 185 +++++++++++++++++++++-
 crates/gpui_windows/src/events.rs                |  30 ++++
 crates/gpui_windows/src/gpui_windows.rs          |   2 +
 crates/gpui_windows/src/keyboard.rs              |  71 ++++-----
 crates/gpui_windows/src/native_drag.rs           | 522 +++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++
 crates/gpui_windows/src/window.rs                |  45 ++++++
 43 files changed, 3537 insertions(+), 186 deletions(-)
```

_Last generated: 2026-06-17 (commit `c472416104` at branch HEAD)._
