//! Shared mask-space post-processing for glow glyph variants.
//!
//! A glow glyph (see [`RenderGlyphParams::embolden`](crate::RenderGlyphParams))
//! is rasterized as a single-channel alpha mask, morphologically dilated by the
//! embolden strength, then Gaussian-blurred. These helpers implement the
//! dilation/blur identically for every platform text system so the glow looks
//! the same on macOS, Windows, and Linux. Backends whose rasterizer already
//! provides a native embolden (e.g. swash on the wgpu backend) may skip
//! [`embolden_alpha_mask`] and only apply [`blur_alpha_mask`].

use crate::RenderGlyphParams;

/// Number of device pixels of padding to reserve around a glyph's raster
/// bounds for the glow post-processing (embolden + Gaussian blur). Returns 0
/// for non-glow glyphs. Platform `raster_bounds` implementations must expand
/// the glyph bounds by this amount on every side, or the dilated/blurred mask
/// clips at the tile edge.
pub fn glow_padding_pixels(params: &RenderGlyphParams) -> i32 {
    let Some(glow) = &params.embolden else {
        return 0;
    };
    let embolden_px = (glow.embolden * params.scale_factor).ceil() as i32;
    // Gaussian sigma = blur_radius / 2; we keep ~3 sigmas worth of contribution.
    let blur_px = (glow.blur_radius * params.scale_factor * 1.5).ceil() as i32;
    embolden_px + blur_px
}

/// Morphological dilation via a separable square max filter of side `2 * radius + 1`.
/// Approximates outline emboldening on a single-channel alpha mask.
pub fn embolden_alpha_mask(data: &mut [u8], width: usize, height: usize, radius: usize) {
    if radius == 0 || width == 0 || height == 0 {
        return;
    }
    let mut temp = vec![0u8; width * height];
    // Horizontal pass: data → temp
    for y in 0..height {
        let row = y * width;
        for x in 0..width {
            let lo = x.saturating_sub(radius);
            let hi = (x + radius).min(width - 1);
            let mut m = 0u8;
            for sx in lo..=hi {
                m = m.max(data[row + sx]);
            }
            temp[row + x] = m;
        }
    }
    // Vertical pass: temp → data
    for y in 0..height {
        let lo = y.saturating_sub(radius);
        let hi = (y + radius).min(height - 1);
        for x in 0..width {
            let mut m = 0u8;
            for sy in lo..=hi {
                m = m.max(temp[sy * width + x]);
            }
            data[y * width + x] = m;
        }
    }
}

/// Separable Gaussian blur on a single-channel alpha mask.
pub fn blur_alpha_mask(data: &mut [u8], width: usize, height: usize, radius: f32) {
    if width == 0 || height == 0 || radius < 0.5 {
        return;
    }
    let sigma = radius / 2.0;
    let kernel_radius = (sigma * 3.0).ceil() as usize;
    if kernel_radius == 0 {
        return;
    }
    // Build 1D Gaussian kernel.
    let kernel_size = kernel_radius * 2 + 1;
    let mut kernel = vec![0.0_f32; kernel_size];
    let mut sum = 0.0_f32;
    for i in 0..kernel_size {
        let x = i as f32 - kernel_radius as f32;
        let val = (-x * x / (2.0 * sigma * sigma)).exp();
        kernel[i] = val;
        sum += val;
    }
    for v in &mut kernel {
        *v /= sum;
    }

    let mut temp = vec![0.0_f32; width * height];

    // Horizontal pass: data → temp
    for y in 0..height {
        for x in 0..width {
            let mut acc = 0.0_f32;
            for k in 0..kernel_size {
                let sx = x as isize + k as isize - kernel_radius as isize;
                let sx = sx.clamp(0, width as isize - 1) as usize;
                acc += data[y * width + sx] as f32 * kernel[k];
            }
            temp[y * width + x] = acc;
        }
    }

    // Vertical pass: temp → data
    for y in 0..height {
        for x in 0..width {
            let mut acc = 0.0_f32;
            for k in 0..kernel_size {
                let sy = y as isize + k as isize - kernel_radius as isize;
                let sy = sy.clamp(0, height as isize - 1) as usize;
                acc += temp[sy * width + x] * kernel[k];
            }
            data[y * width + x] = acc.round() as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FontId, GlowParams, GlyphId, Pixels, point};

    fn glow_params(embolden: f32, blur_radius: f32, scale_factor: f32) -> RenderGlyphParams {
        RenderGlyphParams {
            font_id: FontId(0),
            glyph_id: GlyphId(0),
            font_size: Pixels(14.0),
            subpixel_variant: point(0, 0),
            scale_factor,
            is_emoji: false,
            subpixel_rendering: false,
            embolden: Some(GlowParams {
                embolden,
                blur_radius,
            }),
            dilation: 0,
        }
    }

    #[test]
    fn padding_is_zero_without_glow() {
        let mut params = glow_params(1.2, 3.0, 2.0);
        params.embolden = None;
        assert_eq!(glow_padding_pixels(&params), 0);
    }

    #[test]
    fn padding_covers_embolden_plus_blur() {
        // embolden 1.2 * 2.0 = 2.4 → 3; blur 3.0 * 2.0 * 1.5 = 9.0 → 9
        assert_eq!(glow_padding_pixels(&glow_params(1.2, 3.0, 2.0)), 12);
    }

    #[test]
    fn embolden_dilates_single_pixel() {
        let mut mask = vec![0u8; 25];
        mask[12] = 255; // center of 5x5
        embolden_alpha_mask(&mut mask, 5, 5, 1);
        // Full 3x3 neighborhood becomes opaque, corners of the mask stay empty.
        for y in 1..=3 {
            for x in 1..=3 {
                assert_eq!(mask[y * 5 + x], 255, "({x},{y})");
            }
        }
        assert_eq!(mask[0], 0);
        assert_eq!(mask[24], 0);
    }

    #[test]
    fn blur_spreads_and_preserves_finite_mass() {
        let mut mask = vec![0u8; 81];
        mask[40] = 255; // center of 9x9
        blur_alpha_mask(&mut mask, 9, 9, 2.0);
        assert!(mask[40] < 255, "center should lose intensity");
        assert!(mask[41] > 0, "neighbors should gain intensity");
    }

    #[test]
    fn blur_below_threshold_is_noop() {
        let mut mask = vec![0u8; 9];
        mask[4] = 200;
        blur_alpha_mask(&mut mask, 3, 3, 0.4);
        assert_eq!(mask[4], 200);
        assert_eq!(mask[0], 0);
    }
}
