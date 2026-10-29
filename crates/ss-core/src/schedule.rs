use chrono::{DateTime, Datelike, Local, TimeZone, Timelike};
use serde::{Deserialize, Serialize};

/// Half-hour slots, as in Surveillance Station's weekly schedule grid.
pub const SLOTS_PER_DAY: usize = 48;
pub const SLOTS_PER_WEEK: usize = SLOTS_PER_DAY * 7;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleMode {
    /// Do not record.
    None,
    /// Record all the time.
    Continuous,
    /// Record only around motion events.
    Motion,
    /// Record continuously and additionally flag motion segments.
    ContinuousAndMotion,
}

impl ScheduleMode {
    pub fn records_continuously(self) -> bool {
        matches!(self, Self::Continuous | Self::ContinuousAndMotion)
    }
    pub fn records_on_motion(self) -> bool {
        matches!(self, Self::Motion | Self::ContinuousAndMotion)
    }
}

/// Weekly recording schedule: 7 days (Monday = 0) × 48 half-hour slots,
/// evaluated in the server's local time zone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<ScheduleMode>", into = "Vec<ScheduleMode>")]
pub struct Schedule {
    slots: Vec<ScheduleMode>,
}

impl Default for Schedule {
    fn default() -> Self {
        Self::uniform(ScheduleMode::Continuous)
    }
}

impl Schedule {
    pub fn uniform(mode: ScheduleMode) -> Self {
        Self { slots: vec![mode; SLOTS_PER_WEEK] }
    }

    /// `weekday`: 0 = Monday … 6 = Sunday; `slot`: 0..48 (slot 0 = 00:00–00:30).
    pub fn get(&self, weekday: usize, slot: usize) -> ScheduleMode {
        self.slots[weekday * SLOTS_PER_DAY + slot]
    }

    pub fn set(&mut self, weekday: usize, slot: usize, mode: ScheduleMode) {
        self.slots[weekday * SLOTS_PER_DAY + slot] = mode;
    }

    /// Mode in effect at `t` (converted to the local time zone).
    pub fn mode_at<Tz: TimeZone>(&self, t: &DateTime<Tz>) -> ScheduleMode {
        let local = t.with_timezone(&Local);
        self.mode_at_naive(local.weekday().num_days_from_monday() as usize, local.hour(), local.minute())
    }

    pub fn mode_at_naive(&self, weekday: usize, hour: u32, minute: u32) -> ScheduleMode {
        self.get(weekday, (hour * 2 + minute / 30) as usize)
    }

    pub fn slots(&self) -> &[ScheduleMode] {
        &self.slots
    }
}

impl TryFrom<Vec<ScheduleMode>> for Schedule {
    type Error = String;
    fn try_from(slots: Vec<ScheduleMode>) -> Result<Self, Self::Error> {
        if slots.len() != SLOTS_PER_WEEK {
            return Err(format!("schedule must have {SLOTS_PER_WEEK} slots, got {}", slots.len()));
        }
        Ok(Self { slots })
    }
}

impl From<Schedule> for Vec<ScheduleMode> {
    fn from(s: Schedule) -> Self {
        s.slots
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_lookup() {
        let mut s = Schedule::uniform(ScheduleMode::None);
        s.set(2, 19, ScheduleMode::Motion); // Wednesday 09:30–10:00
        assert_eq!(s.mode_at_naive(2, 9, 45), ScheduleMode::Motion);
        assert_eq!(s.mode_at_naive(2, 10, 0), ScheduleMode::None);
    }

    #[test]
    fn serde_rejects_wrong_length() {
        assert!(serde_json::from_str::<Schedule>("[\"none\"]").is_err());
        let json = serde_json::to_string(&Schedule::default()).unwrap();
        assert_eq!(serde_json::from_str::<Schedule>(&json).unwrap(), Schedule::default());
    }
}
