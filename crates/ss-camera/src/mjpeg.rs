//! Incremental splitter that extracts complete JPEG images (SOI `FFD8` …
//! EOI `FFD9`) from an arbitrary byte stream. Works for HTTP
//! `multipart/x-mixed-replace` bodies (boundaries/headers are skipped) and for
//! ffmpeg `image2pipe` output alike.

use bytes::Bytes;

/// Images larger than this are discarded (protects against garbage streams).
pub const MAX_JPEG_BYTES: usize = 16 * 1024 * 1024;

#[derive(Default)]
pub struct JpegSplitter {
    buf: Vec<u8>,
}

impl JpegSplitter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed bytes; returns every complete JPEG found so far, in order.
    pub fn push(&mut self, data: &[u8]) -> Vec<Bytes> {
        let _ = data;
        todo!()
    }
}
