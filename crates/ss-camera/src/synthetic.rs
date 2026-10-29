//! Generated test pattern; lets the whole system run without a camera.

use std::time::Duration;

use async_trait::async_trait;
use image::codecs::jpeg::JpegEncoder;
use image::{ExtendedColorType, ImageEncoder, Rgb, RgbImage};
use ss_core::{CameraId, Error, Frame, Result};
use tokio::time::{interval, Interval, MissedTickBehavior};

use crate::{make_frame, FrameSource};

/// JPEG quality used for generated frames.
const QUALITY: u8 = 70;
/// The box travels across the frame once every this many seconds.
const PERIOD_SECS: u64 = 5;
/// Every `CYCLE_SECS` seconds of frames, the box moves for `MOVING_SECS` and pauses for the rest.
const CYCLE_SECS: u64 = 10;
const MOVING_SECS: u64 = 7;

pub struct SyntheticSource {
    camera_id: CameraId,
    width: u32,
    height: u32,
    fps: u32,
    period: Duration,
    /// Index of the next frame to render.
    n: u64,
    /// Created on first use so that construction does not require a runtime.
    ticker: Option<Interval>,
}

impl SyntheticSource {
    pub fn new(camera_id: CameraId, width: u32, height: u32, fps: u32) -> Self {
        let width = width.max(1);
        let height = height.max(1);
        let fps = fps.max(1);
        Self {
            camera_id,
            width,
            height,
            fps,
            period: Duration::from_secs_f64(1.0 / f64::from(fps)),
            n: 0,
            ticker: None,
        }
    }

    /// Render frame number `n` as JPEG (deterministic; a box moves across a gradient).
    pub fn render(&self, n: u64) -> Vec<u8> {
        let fps = u64::from(self.fps);
        let cycle = CYCLE_SECS * fps;
        let moving_per_cycle = MOVING_SECS * fps;
        let period = (PERIOD_SECS * fps).max(1);

        // Frames rem 0..MOVING-1 each advance the box; the rest of the cycle repeats the last position.
        let moving_frames = (n / cycle) * moving_per_cycle + (n % cycle).min(moving_per_cycle - 1);
        let phase = moving_frames % period;

        let (w, h) = (self.width, self.height);
        let box_w = (w / 6).max(1);
        let box_h = (h / 6).max(1);
        let travel = u64::from(w.saturating_sub(box_w));
        let box_x = u32::try_from(phase * travel / period).unwrap_or(0);
        let box_y = h.saturating_sub(box_h) / 2;

        let img = RgbImage::from_fn(w, h, |x, y| {
            if (box_x..box_x + box_w).contains(&x) && (box_y..box_y + box_h).contains(&y) {
                return Rgb([255, 255, 255]);
            }
            let r = if w > 1 { x * 255 / (w - 1) } else { 0 };
            let g = if h > 1 { y * 255 / (h - 1) } else { 0 };
            Rgb([r as u8, g as u8, 128])
        });

        let mut out = Vec::new();
        // Encoding into a Vec cannot fail for valid dimensions; an empty result signals failure.
        if JpegEncoder::new_with_quality(&mut out, QUALITY)
            .write_image(img.as_raw(), w, h, ExtendedColorType::Rgb8)
            .is_err()
        {
            out.clear();
        }
        out
    }
}

#[async_trait]
impl FrameSource for SyntheticSource {
    async fn next_frame(&mut self) -> Result<Frame> {
        let period = self.period;
        let ticker = self.ticker.get_or_insert_with(|| {
            let mut t = interval(period);
            t.set_missed_tick_behavior(MissedTickBehavior::Delay);
            t
        });
        ticker.tick().await;
        let jpeg = self.render(self.n);
        self.n = self.n.wrapping_add(1);
        if jpeg.is_empty() {
            return Err(Error::Internal("synthetic frame encoding failed".into()));
        }
        Ok(make_frame(self.camera_id, jpeg.into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jpeg;

    fn decode(jpeg_bytes: &[u8]) -> RgbImage {
        image::load_from_memory_with_format(jpeg_bytes, image::ImageFormat::Jpeg)
            .expect("decode")
            .to_rgb8()
    }

    #[test]
    fn renders_correct_size_and_is_decodable() {
        let s = SyntheticSource::new(1, 320, 240, 10);
        let bytes = s.render(0);
        assert_eq!(jpeg::dimensions(&bytes), Some((320, 240)));
        let img = decode(&bytes);
        assert_eq!(img.dimensions(), (320, 240));
    }

    #[test]
    fn consecutive_frames_differ_while_moving() {
        let s = SyntheticSource::new(1, 160, 120, 10);
        assert_ne!(s.render(0), s.render(1));
        assert_ne!(s.render(10), s.render(11));
    }

    #[test]
    fn box_pauses_for_three_seconds_each_cycle() {
        let s = SyntheticSource::new(1, 160, 120, 10);
        // Frames 70..100 are the pause (3 s at 10 fps) and must be identical.
        assert_eq!(s.render(70), s.render(99));
        // Box moves again afterwards.
        assert_ne!(s.render(99), s.render(100));
        assert_eq!(s.render(170), s.render(199));
        assert_ne!(s.render(199), s.render(200));
    }

    #[test]
    fn tiny_sizes_do_not_panic() {
        let s = SyntheticSource::new(1, 1, 1, 1);
        assert_eq!(jpeg::dimensions(&s.render(3)), Some((1, 1)));
    }

    #[tokio::test]
    async fn next_frame_is_paced_and_stamped() {
        let mut s = SyntheticSource::new(7, 640, 480, 50);
        let start = std::time::Instant::now();
        let a = s.next_frame().await.expect("frame");
        let b = s.next_frame().await.expect("frame");
        assert_eq!(a.camera_id, 7);
        assert_eq!((a.width, a.height), (640, 480));
        assert_ne!(a.jpeg, b.jpeg);
        assert!(start.elapsed() >= Duration::from_millis(15));
    }
}
