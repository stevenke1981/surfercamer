//! Incremental splitter that extracts complete JPEG images (SOI `FFD8` …
//! EOI `FFD9`) from an arbitrary byte stream. Works for HTTP
//! `multipart/x-mixed-replace` bodies (boundaries/headers are skipped) and for
//! ffmpeg `image2pipe` output alike.

use std::collections::VecDeque;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::stream::BoxStream;
use futures_util::StreamExt;
use ss_core::{CameraId, Frame, Result};
use tokio::time::Instant;

use crate::http::{build_client, get_with_auth, CONNECT_TIMEOUT};
use crate::{camera_error, make_frame, FrameSource};

/// Images larger than this are discarded (protects against garbage streams).
pub const MAX_JPEG_BYTES: usize = 16 * 1024 * 1024;

/// A stream with no complete frame for this long is treated as broken.
const NO_FRAME_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Default)]
pub struct JpegSplitter {
    buf: Vec<u8>,
    /// True once a SOI has been found and the buffer starts with it.
    in_frame: bool,
    /// Index in `buf` from which the next EOI search starts.
    scanned: usize,
}

/// Position of the first `FF <second>` pair at or after `from`.
fn find_pair(buf: &[u8], from: usize, second: u8) -> Option<usize> {
    buf.get(from..)?
        .windows(2)
        .position(|w| w[0] == 0xFF && w[1] == second)
        .map(|p| p + from)
}

impl JpegSplitter {
    pub fn new() -> Self {
        Self::default()
    }

    fn reset(&mut self) {
        self.buf.clear();
        self.in_frame = false;
        self.scanned = 0;
    }

    /// Feed bytes; returns every complete JPEG found so far, in order.
    pub fn push(&mut self, data: &[u8]) -> Vec<Bytes> {
        self.buf.extend_from_slice(data);
        let mut out = Vec::new();
        loop {
            if !self.in_frame {
                match find_pair(&self.buf, 0, 0xD8) {
                    Some(start) => {
                        self.buf.drain(..start);
                        self.in_frame = true;
                        self.scanned = 2;
                    }
                    None => {
                        // Keep a trailing 0xFF: it may be the first half of a split SOI.
                        let keep = usize::from(self.buf.last() == Some(&0xFF));
                        let drop_to = self.buf.len() - keep;
                        self.buf.drain(..drop_to);
                        break;
                    }
                }
            }
            match find_pair(&self.buf, self.scanned, 0xD9) {
                Some(end) => {
                    let frame: Vec<u8> = self.buf.drain(..end + 2).collect();
                    self.in_frame = false;
                    self.scanned = 0;
                    if frame.len() <= MAX_JPEG_BYTES {
                        out.push(Bytes::from(frame));
                    }
                }
                None => {
                    if self.buf.len() > MAX_JPEG_BYTES {
                        self.reset();
                    } else {
                        // The last byte may be the first half of a split EOI.
                        self.scanned = self.buf.len().saturating_sub(1);
                    }
                    break;
                }
            }
        }
        out
    }
}

pub struct MjpegHttpSource {
    camera_id: CameraId,
    stream: BoxStream<'static, reqwest::Result<Bytes>>,
    splitter: JpegSplitter,
    pending: VecDeque<Bytes>,
    last_frame: Instant,
}

impl MjpegHttpSource {
    pub async fn connect(
        camera_id: CameraId,
        url: &str,
        username: Option<&str>,
        password: Option<&str>,
    ) -> Result<Self> {
        let client = build_client(CONNECT_TIMEOUT)?;
        let response = get_with_auth(&client, url, username, password).await?;
        Ok(Self {
            camera_id,
            stream: response.bytes_stream().boxed(),
            splitter: JpegSplitter::new(),
            pending: VecDeque::new(),
            last_frame: Instant::now(),
        })
    }
}

#[async_trait]
impl FrameSource for MjpegHttpSource {
    async fn next_frame(&mut self) -> Result<Frame> {
        loop {
            if let Some(jpeg) = self.pending.pop_front() {
                self.last_frame = Instant::now();
                return Ok(make_frame(self.camera_id, jpeg));
            }
            let deadline = self.last_frame + NO_FRAME_TIMEOUT;
            match tokio::time::timeout_at(deadline, self.stream.next()).await {
                Err(_) => return Err(camera_error("no frame received for 15 s")),
                Ok(None) => return Err(camera_error("stream ended")),
                Ok(Some(Err(e))) => return Err(camera_error(format!("stream error: {e}"))),
                Ok(Some(Ok(chunk))) => self.pending.extend(self.splitter.push(&chunk)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jpeg(tag: u8) -> Vec<u8> {
        // Not decoded by the splitter: any bytes between SOI and EOI work.
        let mut v = vec![0xFF, 0xD8, 0xFF, 0xE0, tag, tag];
        v.extend_from_slice(&[tag; 10]);
        v.extend_from_slice(&[0xFF, 0xD9]);
        v
    }

    fn all_frames(chunks: &[&[u8]]) -> Vec<Vec<u8>> {
        let mut s = JpegSplitter::new();
        let mut out = Vec::new();
        for c in chunks {
            out.extend(s.push(c).into_iter().map(|b| b.to_vec()));
        }
        out
    }

    #[test]
    fn splits_multipart_stream() {
        let mut body = Vec::new();
        for tag in [1u8, 2, 3] {
            body.extend_from_slice(
                b"--frame\r\nContent-Type: image/jpeg\r\nContent-Length: 20\r\n\r\n",
            );
            body.extend_from_slice(&jpeg(tag));
            body.extend_from_slice(b"\r\n");
        }
        let frames = all_frames(&[&body]);
        assert_eq!(frames, vec![jpeg(1), jpeg(2), jpeg(3)]);
    }

    #[test]
    fn handles_markers_split_across_pushes() {
        let mut body = b"--x\r\n\r\n".to_vec();
        body.extend_from_slice(&jpeg(7));
        body.extend_from_slice(&jpeg(8));
        // Split every byte apart: the worst case for marker boundaries.
        let chunks: Vec<&[u8]> = body.chunks(1).collect();
        assert_eq!(all_frames(&chunks), vec![jpeg(7), jpeg(8)]);
    }

    #[test]
    fn drops_garbage_before_start_and_between_frames() {
        let mut body = b"garbage\xFF\xFF\x00\xD9 more".to_vec();
        body.extend_from_slice(&jpeg(4));
        body.extend_from_slice(b"\x00\x01noise\xFF");
        body.extend_from_slice(&jpeg(5));
        assert_eq!(all_frames(&[&body]), vec![jpeg(4), jpeg(5)]);
    }

    #[test]
    fn incomplete_frame_is_held_until_end() {
        let mut s = JpegSplitter::new();
        let full = jpeg(9);
        let (head, tail) = full.split_at(full.len() - 3);
        assert!(s.push(head).is_empty());
        assert_eq!(s.push(tail).len(), 1);
    }

    #[test]
    fn oversized_unterminated_frame_is_discarded() {
        let mut s = JpegSplitter::new();
        assert!(s.push(&[0xFF, 0xD8]).is_empty());
        let filler = vec![0u8; 1024 * 1024];
        for _ in 0..(MAX_JPEG_BYTES / filler.len() + 1) {
            assert!(s.push(&filler).is_empty());
        }
        // Buffer was reset; a fresh frame still works.
        assert_eq!(s.push(&jpeg(6)).len(), 1);
    }
}
