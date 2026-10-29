//! Frame-difference motion detection. See docs/SPEC.md §7.

use ss_core::{MotionConfig, Result};

/// Frames are downscaled to this width (aspect preserved) before analysis.
pub const ANALYSIS_WIDTH: u32 = 160;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MotionResult {
    /// Fraction (0.0..=1.0) of analysed pixels that changed.
    pub score: f32,
    /// `score * 100 >= threshold` (always false for the first frame or when disabled).
    pub triggered: bool,
}

/// Map sensitivity 1..=99 to a per-pixel luma delta: 99 → 5, 1 → 100 (linear).
pub fn sensitivity_to_delta(sensitivity: u8) -> u8 {
    let _ = sensitivity;
    todo!()
}

pub struct MotionDetector {
    config: MotionConfig,
}

impl MotionDetector {
    pub fn new(config: MotionConfig) -> Self {
        Self { config }
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
        todo!()
    }

    /// Decode a JPEG, convert to luma, downscale and analyse.
    pub fn process_jpeg(&mut self, jpeg: &[u8]) -> Result<MotionResult> {
        let _ = jpeg;
        todo!()
    }

    /// Analyse an 8-bit luma image (already at analysis resolution or not).
    pub fn process_luma(&mut self, width: u32, height: u32, luma: &[u8]) -> MotionResult {
        let _ = (width, height, luma);
        todo!()
    }
}
