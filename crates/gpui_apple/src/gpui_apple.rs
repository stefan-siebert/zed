#![cfg(target_os = "macos")]
//! Shared Apple platform support for GPUI.
//!
//! This crate contains the Metal renderer and GPU resource management shared
//! by GPUI's Apple platform backends.

mod metal_atlas;
pub mod metal_renderer;

/// Runtime for caller-provided WGSL fragments (this fork's custom-shader
/// feature). Lives here rather than in `gpui_macos` because
/// `metal_renderer.rs`, its only caller, moved with the crate split.
mod metal_custom_shader;
