//! Decides whether a camera should be recording right now, and why.

use chrono::{DateTime, Duration, Utc};
use ss_core::{RecordingTrigger, ScheduleMode};

#[derive(Debug, Default, Clone)]
pub struct RecordPolicy {
    last_motion: Option<DateTime<Utc>>,
    manual: bool,
}

impl RecordPolicy {
    pub fn new() -> Self {
        Self::default()
    }

    /// Motion detected at `at`. Out-of-order calls never move the window backwards.
    pub fn on_motion(&mut self, at: DateTime<Utc>) {
        self.last_motion = Some(match self.last_motion {
            Some(prev) if prev > at => prev,
            _ => at,
        });
    }

    pub fn set_manual(&mut self, on: bool) {
        self.manual = on;
    }

    pub fn manual(&self) -> bool {
        self.manual
    }

    /// Whether motion recording is active at `now` (last motion within `post_record_seconds`).
    pub fn motion_active(&self, now: DateTime<Utc>, post_record_seconds: u32) -> bool {
        match self.last_motion {
            Some(last) => now - last <= Duration::seconds(i64::from(post_record_seconds)),
            None => false,
        }
    }

    /// Priority: Manual > Motion (mode records on motion and motion active)
    /// > Continuous (mode records continuously) > None.
    pub fn decide(&self, now: DateTime<Utc>, mode: ScheduleMode, post_record_seconds: u32) -> Option<RecordingTrigger> {
        if self.manual {
            return Some(RecordingTrigger::Manual);
        }
        if mode.records_on_motion() && self.motion_active(now, post_record_seconds) {
            return Some(RecordingTrigger::Motion);
        }
        if mode.records_continuously() {
            return Some(RecordingTrigger::Continuous);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    const ALL_MODES: [ScheduleMode; 4] = [
        ScheduleMode::None,
        ScheduleMode::Continuous,
        ScheduleMode::Motion,
        ScheduleMode::ContinuousAndMotion,
    ];

    fn t(sec: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap() + Duration::seconds(sec)
    }

    #[test]
    fn priority_table_without_motion_or_manual() {
        let p = RecordPolicy::new();
        let expected = [None, Some(RecordingTrigger::Continuous), None, Some(RecordingTrigger::Continuous)];
        for (mode, want) in ALL_MODES.into_iter().zip(expected) {
            assert_eq!(p.decide(t(0), mode, 10), want, "mode {mode:?}");
        }
    }

    #[test]
    fn priority_table_with_active_motion() {
        let mut p = RecordPolicy::new();
        p.on_motion(t(100));
        let expected = [None, Some(RecordingTrigger::Continuous), Some(RecordingTrigger::Motion), Some(RecordingTrigger::Motion)];
        for (mode, want) in ALL_MODES.into_iter().zip(expected) {
            assert_eq!(p.decide(t(105), mode, 10), want, "mode {mode:?}");
        }
    }

    #[test]
    fn motion_window_expires() {
        let mut p = RecordPolicy::new();
        p.on_motion(t(100));
        assert!(p.motion_active(t(110), 10)); // exactly at the boundary
        assert!(!p.motion_active(t(111), 10));
        assert_eq!(p.decide(t(111), ScheduleMode::Motion, 10), None);
        assert_eq!(p.decide(t(111), ScheduleMode::ContinuousAndMotion, 10), Some(RecordingTrigger::Continuous));
        assert_eq!(p.decide(t(110), ScheduleMode::Motion, 10), Some(RecordingTrigger::Motion));
    }

    #[test]
    fn zero_post_record_only_at_motion_instant() {
        let mut p = RecordPolicy::new();
        p.on_motion(t(50));
        assert!(p.motion_active(t(50), 0));
        assert!(!p.motion_active(t(51), 0));
    }

    #[test]
    fn manual_overrides_every_mode() {
        let mut p = RecordPolicy::new();
        p.set_manual(true);
        assert!(p.manual());
        for mode in ALL_MODES {
            assert_eq!(p.decide(t(0), mode, 0), Some(RecordingTrigger::Manual), "mode {mode:?}");
        }
        p.set_manual(false);
        assert!(!p.manual());
        assert_eq!(p.decide(t(0), ScheduleMode::None, 0), None);
    }

    #[test]
    fn out_of_order_motion_keeps_latest() {
        let mut p = RecordPolicy::new();
        p.on_motion(t(100));
        p.on_motion(t(50));
        assert!(p.motion_active(t(105), 10));
    }
}
