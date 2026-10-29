//! Decides whether a camera should be recording right now, and why.

use chrono::{DateTime, Utc};
use ss_core::{RecordingTrigger, ScheduleMode};

#[derive(Debug, Default, Clone)]
pub struct RecordPolicy {
    _priv: (),
}

impl RecordPolicy {
    pub fn new() -> Self {
        Self::default()
    }

    /// Motion detected at `at`.
    pub fn on_motion(&mut self, at: DateTime<Utc>) {
        let _ = at;
        todo!()
    }

    pub fn set_manual(&mut self, on: bool) {
        let _ = on;
        todo!()
    }

    pub fn manual(&self) -> bool {
        todo!()
    }

    /// Whether motion recording is active at `now` (last motion within `post_record_seconds`).
    pub fn motion_active(&self, now: DateTime<Utc>, post_record_seconds: u32) -> bool {
        let _ = (now, post_record_seconds);
        todo!()
    }

    /// Priority: Manual > Motion (mode records on motion and motion active)
    /// > Continuous (mode records continuously) > None.
    pub fn decide(&self, now: DateTime<Utc>, mode: ScheduleMode, post_record_seconds: u32) -> Option<RecordingTrigger> {
        let _ = (now, mode, post_record_seconds);
        todo!()
    }
}
