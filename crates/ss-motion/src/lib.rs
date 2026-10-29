//! Frame-difference motion detection. See docs/SPEC.md §7.

use image::imageops::{self, FilterType};
use image::{GrayImage, ImageFormat};
use ss_core::{MotionConfig, Region, Result};

/// Frames are downscaled to this width (aspect preserved) before analysis.
pub const ANALYSIS_WIDTH: u32 = 160;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MotionResult {
    /// Fraction (0.0..=1.0) of analysed pixels that changed.
    pub score: f32,
    /// `score * 100 >= threshold` (always false for the first frame or when disabled).
    pub triggered: bool,
}

const NO_MOTION: MotionResult = MotionResult { score: 0.0, triggered: false };

/// Map sensitivity 1..=99 to a per-pixel luma delta: 99 → 5, 1 → 100 (linear).
pub fn sensitivity_to_delta(sensitivity: u8) -> u8 {
    let s = f32::from(sensitivity.clamp(1, 99));
    let delta = 100.0 - (s - 1.0) * 95.0 / 98.0;
    delta.round() as u8
}

pub struct MotionDetector {
    config: MotionConfig,
    /// Background model at analysis resolution; empty until the first frame.
    bg: Vec<f32>,
    width: u32,
    height: u32,
    /// Reused per-frame mask of analysed pixels (only populated when regions are set).
    mask: Vec<bool>,
}

impl MotionDetector {
    pub fn new(config: MotionConfig) -> Self {
        Self { config, bg: Vec::new(), width: 0, height: 0, mask: Vec::new() }
    }

    pub fn config(&self) -> &MotionConfig {
        &self.config
    }

    /// Replace configuration; keeps the background model.
    pub fn set_config(&mut self, config: MotionConfig) {
        self.config = config;
    }

    /// Forget the background model (e.g. after reconnect).
    pub fn reset(&mut self) {
        self.bg.clear();
        self.width = 0;
        self.height = 0;
    }

    /// Decode a JPEG, convert to luma, downscale and analyse.
    pub fn process_jpeg(&mut self, jpeg: &[u8]) -> Result<MotionResult> {
        let img = image::load_from_memory_with_format(jpeg, ImageFormat::Jpeg)
            .map_err(|e| ss_core::Error::Invalid(format!("jpeg decode failed: {e}")))?;
        let luma = img.to_luma8();
        let (w, h) = luma.dimensions();
        Ok(self.process_luma(w, h, luma.as_raw()))
    }

    /// Analyse an 8-bit luma image (already at analysis resolution or not).
    ///
    /// A buffer whose length does not equal `width * height` (or a zero size)
    /// yields no motion and leaves the background model untouched.
    pub fn process_luma(&mut self, width: u32, height: u32, luma: &[u8]) -> MotionResult {
        let expected = (width as usize).checked_mul(height as usize);
        if width == 0 || height == 0 || expected != Some(luma.len()) {
            return NO_MOTION;
        }
        if width == ANALYSIS_WIDTH {
            return self.analyse(width, height, luma);
        }
        let w64 = u64::from(width);
        let nh = ((u64::from(height) * u64::from(ANALYSIS_WIDTH) + w64 / 2) / w64).max(1) as u32;
        let Some(img) = GrayImage::from_raw(width, height, luma.to_vec()) else {
            return NO_MOTION;
        };
        let small = imageops::resize(&img, ANALYSIS_WIDTH, nh, FilterType::Triangle);
        self.analyse(ANALYSIS_WIDTH, nh, small.as_raw())
    }

    /// Core analysis on a buffer already at `w x h` analysis resolution.
    fn analyse(&mut self, w: u32, h: u32, luma: &[u8]) -> MotionResult {
        if self.width != w || self.height != h || self.bg.len() != luma.len() {
            self.width = w;
            self.height = h;
            self.bg = luma.iter().map(|&p| f32::from(p)).collect();
            return NO_MOTION;
        }

        let delta = f32::from(sensitivity_to_delta(self.config.sensitivity));
        let full = self.config.regions.is_empty();
        if !full {
            build_mask(&mut self.mask, w, h, &self.config.regions);
        }

        let mask = &self.mask;
        let mut changed: u64 = 0;
        let mut analysed: u64 = 0;
        for (i, (bg, &p)) in self.bg.iter_mut().zip(luma).enumerate() {
            let cur = f32::from(p);
            if full || mask[i] {
                analysed += 1;
                if (cur - *bg).abs() > delta {
                    changed += 1;
                }
            }
            // Background is compared before it is updated.
            *bg = *bg * 0.9 + cur * 0.1;
        }

        if analysed == 0 {
            return NO_MOTION;
        }
        let score = changed as f32 / analysed as f32;
        let over = changed * 100 >= u64::from(self.config.threshold) * analysed;
        MotionResult { score, triggered: self.config.enabled && over }
    }
}

/// Fill `mask` (one flag per pixel, row-major) with the union of `regions`.
fn build_mask(mask: &mut Vec<bool>, w: u32, h: u32, regions: &[Region]) {
    mask.clear();
    mask.resize(w as usize * h as usize, false);
    let (wf, hf) = (w as f32, h as f32);
    for r in regions {
        let x0 = clamp_px((r.x * wf).floor(), w);
        let x1 = clamp_px(((r.x + r.w) * wf).ceil(), w).max(x0);
        let y0 = clamp_px((r.y * hf).floor(), h);
        let y1 = clamp_px(((r.y + r.h) * hf).ceil(), h).max(y0);
        for y in y0..y1 {
            let row = y as usize * w as usize;
            mask[row + x0 as usize..row + x1 as usize].fill(true);
        }
    }
}

fn clamp_px(v: f32, max: u32) -> u32 {
    v.max(0.0).min(max as f32) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::codecs::jpeg::JpegEncoder;

    const W: u32 = 160;
    const H: u32 = 120;

    /// Uniform `base` frame with an optional filled rectangle `(x, y, rw, rh)` of `value`.
    fn frame(w: u32, h: u32, base: u8, rect: Option<(u32, u32, u32, u32)>, value: u8) -> Vec<u8> {
        let mut v = vec![base; (w * h) as usize];
        if let Some((x, y, rw, rh)) = rect {
            for yy in y..y + rh {
                for xx in x..x + rw {
                    v[(yy * w + xx) as usize] = value;
                }
            }
        }
        v
    }

    fn det() -> MotionDetector {
        MotionDetector::new(MotionConfig::default())
    }

    fn det_with(config: MotionConfig) -> MotionDetector {
        MotionDetector::new(config)
    }

    fn region(x: f32, y: f32, w: f32, h: f32) -> Region {
        Region { x, y, w, h }
    }

    #[test]
    fn delta_mapping_endpoints() {
        assert_eq!(sensitivity_to_delta(1), 100);
        assert_eq!(sensitivity_to_delta(99), 5);
        assert_eq!(sensitivity_to_delta(0), 100);
        assert_eq!(sensitivity_to_delta(200), 5);
        assert!((52..=53).contains(&sensitivity_to_delta(50)));
    }

    #[test]
    fn first_frame_is_no_motion() {
        let mut d = det();
        let r = d.process_luma(W, H, &frame(W, H, 20, None, 0));
        assert_eq!(r, NO_MOTION);
    }

    #[test]
    fn identical_frames_score_zero() {
        let mut d = det();
        let f = frame(W, H, 90, None, 0);
        d.process_luma(W, H, &f);
        let r = d.process_luma(W, H, &f);
        assert_eq!(r.score, 0.0);
        assert!(!r.triggered);
    }

    #[test]
    fn big_bright_square_triggers() {
        let mut d = det();
        d.process_luma(W, H, &frame(W, H, 20, None, 0));
        let r = d.process_luma(W, H, &frame(W, H, 20, Some((40, 30, 80, 60)), 220));
        // 80*60 of 160*120 pixels changed.
        assert!((r.score - 0.25).abs() < 1e-4, "score {}", r.score);
        assert!(r.triggered);
    }

    #[test]
    fn small_change_below_threshold_not_triggered() {
        let mut d = det();
        d.process_luma(W, H, &frame(W, H, 20, None, 0));
        let r = d.process_luma(W, H, &frame(W, H, 20, Some((10, 10, 4, 4)), 220));
        assert!(r.score > 0.0);
        assert!(!r.triggered);
    }

    #[test]
    fn threshold_boundary_is_inclusive() {
        // 25% change against threshold 25 must trigger (score*100 >= threshold).
        let config = MotionConfig { threshold: 25, ..MotionConfig::default() };
        let mut d = det_with(config);
        d.process_luma(W, H, &frame(W, H, 20, None, 0));
        let r = d.process_luma(W, H, &frame(W, H, 20, Some((40, 30, 80, 60)), 220));
        assert!(r.triggered);
    }

    #[test]
    fn region_excluding_change_not_triggered() {
        let config = MotionConfig { regions: vec![region(0.5, 0.0, 0.5, 1.0)], ..MotionConfig::default() };
        let mut d = det_with(config);
        let change = frame(W, H, 20, Some((0, 0, 80, 60)), 220);
        d.process_luma(W, H, &frame(W, H, 20, None, 0));
        let r = d.process_luma(W, H, &change);
        assert_eq!(r.score, 0.0);
        assert!(!r.triggered);
    }

    #[test]
    fn region_including_change_triggered() {
        let config = MotionConfig { regions: vec![region(0.0, 0.0, 0.5, 0.5)], ..MotionConfig::default() };
        let mut d = det_with(config);
        d.process_luma(W, H, &frame(W, H, 20, None, 0));
        let r = d.process_luma(W, H, &frame(W, H, 20, Some((0, 0, 80, 60)), 220));
        assert!((r.score - 1.0).abs() < 1e-6);
        assert!(r.triggered);
    }

    #[test]
    fn overlapping_regions_counted_once() {
        let single = MotionConfig { regions: vec![region(0.0, 0.0, 0.5, 1.0)], ..MotionConfig::default() };
        let dup = MotionConfig {
            regions: vec![region(0.0, 0.0, 0.5, 1.0), region(0.0, 0.0, 0.5, 1.0)],
            ..MotionConfig::default()
        };
        let change = frame(W, H, 20, Some((0, 0, 40, 40)), 220);
        let base = frame(W, H, 20, None, 0);

        let mut a = det_with(single);
        a.process_luma(W, H, &base);
        let sa = a.process_luma(W, H, &change).score;

        let mut b = det_with(dup);
        b.process_luma(W, H, &base);
        let sb = b.process_luma(W, H, &change).score;

        assert!((sa - sb).abs() < 1e-6, "single {sa} vs duplicated {sb}");
        assert!(sb > 0.0);

        // Union of two overlapping regions covers the whole frame: full change scores 1.0.
        let overlap = MotionConfig {
            regions: vec![region(0.0, 0.0, 0.6, 1.0), region(0.4, 0.0, 0.6, 1.0)],
            ..MotionConfig::default()
        };
        let mut c = det_with(overlap);
        c.process_luma(W, H, &base);
        let sc = c.process_luma(W, H, &frame(W, H, 220, None, 0)).score;
        assert!((sc - 1.0).abs() < 1e-6, "overlap score {sc}");
    }

    #[test]
    fn disabled_never_triggers() {
        let config = MotionConfig { enabled: false, ..MotionConfig::default() };
        let mut d = det_with(config);
        d.process_luma(W, H, &frame(W, H, 20, None, 0));
        let r = d.process_luma(W, H, &frame(W, H, 20, Some((40, 30, 80, 60)), 220));
        assert!(r.score > 0.2);
        assert!(!r.triggered);
    }

    #[test]
    fn size_change_resets_background() {
        let mut d = det();
        d.process_luma(W, H, &frame(W, H, 20, None, 0));
        // Different analysed height: background is re-initialised.
        let small = frame(W, 90, 220, None, 0);
        assert_eq!(d.process_luma(W, 90, &small), NO_MOTION);
        assert_eq!(d.process_luma(W, 90, &small).score, 0.0);
    }

    #[test]
    fn non_analysis_width_is_downscaled() {
        let mut d = det();
        d.process_luma(320, 240, &frame(320, 240, 20, None, 0));
        let r = d.process_luma(320, 240, &frame(320, 240, 20, Some((0, 0, 160, 120)), 220));
        assert!((r.score - 0.25).abs() < 0.05, "score {}", r.score);
        assert!(r.triggered);
    }

    #[test]
    fn reset_forgets_background() {
        let mut d = det();
        d.process_luma(W, H, &frame(W, H, 20, None, 0));
        d.reset();
        let r = d.process_luma(W, H, &frame(W, H, 220, None, 0));
        assert_eq!(r, NO_MOTION);
    }

    #[test]
    fn invalid_luma_length_is_no_motion() {
        let mut d = det();
        assert_eq!(d.process_luma(W, H, &[0u8; 10]), NO_MOTION);
        assert_eq!(d.process_luma(0, 0, &[]), NO_MOTION);
        d.process_luma(W, H, &frame(W, H, 20, None, 0));
        assert_eq!(d.process_luma(W, H, &[0u8; 10]), NO_MOTION);
    }

    fn encode_jpeg(w: u32, h: u32, pixels: Vec<u8>) -> Vec<u8> {
        let img = GrayImage::from_raw(w, h, pixels).expect("buffer size");
        let mut out = Vec::new();
        JpegEncoder::new_with_quality(&mut out, 95).encode_image(&img).expect("encode");
        out
    }

    #[test]
    fn process_jpeg_detects_motion() {
        let mut d = det();
        let still = encode_jpeg(W, H, frame(W, H, 20, None, 0));
        let moved = encode_jpeg(W, H, frame(W, H, 20, Some((40, 30, 80, 60)), 220));
        assert_eq!(d.process_jpeg(&still).expect("decode"), NO_MOTION);
        let r = d.process_jpeg(&moved).expect("decode");
        assert!(r.score > 0.2, "score {}", r.score);
        assert!(r.triggered);
    }

    #[test]
    fn process_jpeg_rejects_invalid_bytes() {
        let mut d = det();
        assert!(matches!(d.process_jpeg(b"definitely not a jpeg"), Err(ss_core::Error::Invalid(_))));
        assert!(matches!(d.process_jpeg(&[]), Err(ss_core::Error::Invalid(_))));
    }
}
