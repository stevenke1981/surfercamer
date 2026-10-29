//! Ring buffer of recent frames, flushed into a new motion segment.

use bytes::Bytes;
use chrono::{DateTime, Utc};

pub struct PreBuffer {
    _priv: (),
}

impl PreBuffer {
    /// Keep frames no older than `seconds` relative to the newest frame.
    pub fn new(seconds: u32) -> Self {
        let _ = seconds;
        todo!()
    }
    pub fn set_seconds(&mut self, seconds: u32) {
        let _ = seconds;
        todo!()
    }
    pub fn push(&mut self, ts: DateTime<Utc>, jpeg: Bytes) {
        let _ = (ts, jpeg);
        todo!()
    }
    /// Remove and return all buffered frames, oldest first.
    pub fn drain(&mut self) -> Vec<(DateTime<Utc>, Bytes)> {
        todo!()
    }
    pub fn len(&self) -> usize {
        todo!()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
