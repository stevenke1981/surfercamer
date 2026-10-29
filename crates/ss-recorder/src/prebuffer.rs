//! Ring buffer of recent frames, flushed into a new motion segment.

use std::collections::VecDeque;

use bytes::Bytes;
use chrono::{DateTime, Duration, Utc};

pub struct PreBuffer {
    seconds: u32,
    frames: VecDeque<(DateTime<Utc>, Bytes)>,
}

impl PreBuffer {
    /// Keep frames no older than `seconds` relative to the newest frame.
    pub fn new(seconds: u32) -> Self {
        Self { seconds, frames: VecDeque::new() }
    }

    pub fn set_seconds(&mut self, seconds: u32) {
        self.seconds = seconds;
        if seconds == 0 {
            self.frames.clear();
        } else if let Some(&(newest, _)) = self.frames.back() {
            self.evict_before(newest);
        }
    }

    pub fn push(&mut self, ts: DateTime<Utc>, jpeg: Bytes) {
        if self.seconds == 0 {
            return;
        }
        self.frames.push_back((ts, jpeg));
        self.evict_before(ts);
    }

    /// Drop frames older than `newest - seconds`.
    fn evict_before(&mut self, newest: DateTime<Utc>) {
        let window = Duration::seconds(i64::from(self.seconds));
        while let Some(&(ts, _)) = self.frames.front() {
            if newest - ts > window {
                self.frames.pop_front();
            } else {
                break;
            }
        }
    }

    /// Remove and return all buffered frames, oldest first.
    pub fn drain(&mut self) -> Vec<(DateTime<Utc>, Bytes)> {
        self.frames.drain(..).collect()
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t(sec: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap() + Duration::seconds(sec)
    }

    #[test]
    fn evicts_frames_older_than_window() {
        let mut pb = PreBuffer::new(5);
        for s in 0..=10 {
            pb.push(t(s), Bytes::from(vec![s as u8]));
        }
        // Newest is t(10); keep t(5)..=t(10).
        assert_eq!(pb.len(), 6);
        let drained = pb.drain();
        let times: Vec<_> = drained.iter().map(|(ts, _)| *ts).collect();
        assert_eq!(times, (5..=10).map(t).collect::<Vec<_>>());
        assert_eq!(drained[0].1.as_ref(), &[5u8]);
        assert!(pb.is_empty());
    }

    #[test]
    fn zero_seconds_keeps_nothing() {
        let mut pb = PreBuffer::new(0);
        pb.push(t(0), Bytes::from_static(b"a"));
        pb.push(t(1), Bytes::from_static(b"b"));
        assert!(pb.is_empty());
        assert!(pb.drain().is_empty());
    }

    #[test]
    fn set_seconds_shrinks_and_zero_clears() {
        let mut pb = PreBuffer::new(10);
        for s in 0..=10 {
            pb.push(t(s), Bytes::from_static(b"x"));
        }
        assert_eq!(pb.len(), 11);
        pb.set_seconds(2);
        assert_eq!(pb.len(), 3);
        pb.set_seconds(0);
        assert!(pb.is_empty());
        pb.push(t(20), Bytes::from_static(b"x"));
        assert!(pb.is_empty());
    }

    #[test]
    fn drain_empties_and_preserves_order() {
        let mut pb = PreBuffer::new(100);
        pb.push(t(1), Bytes::from_static(b"1"));
        pb.push(t(2), Bytes::from_static(b"2"));
        let first = pb.drain();
        assert_eq!(first.len(), 2);
        assert_eq!(first[0].0, t(1));
        assert_eq!(pb.len(), 0);
        pb.push(t(3), Bytes::from_static(b"3"));
        assert_eq!(pb.len(), 1);
    }
}
