use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::schedule::Schedule;

pub type CameraId = i64;
pub type RecordingId = i64;
pub type EventId = i64;
pub type UserId = i64;

// ---------------------------------------------------------------- cameras

/// Where a camera's frames come from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CameraSource {
    /// RTSP stream, decoded to MJPEG through an `ffmpeg` child process.
    Rtsp {
        url: String,
        #[serde(default)]
        username: Option<String>,
        #[serde(default)]
        password: Option<String>,
    },
    /// HTTP `multipart/x-mixed-replace` MJPEG stream.
    Mjpeg {
        url: String,
        #[serde(default)]
        username: Option<String>,
        #[serde(default)]
        password: Option<String>,
    },
    /// HTTP endpoint returning one JPEG per request, polled every `interval_ms`.
    Snapshot {
        url: String,
        #[serde(default)]
        username: Option<String>,
        #[serde(default)]
        password: Option<String>,
        interval_ms: u64,
    },
    /// Generated test pattern (moving box + timestamp-ish noise). No network.
    Synthetic { width: u32, height: u32, fps: u32 },
}

impl CameraSource {
    /// Copy with credentials removed, for API responses.
    pub fn redacted(&self) -> Self {
        let mut s = self.clone();
        match &mut s {
            CameraSource::Rtsp { url, password, .. }
            | CameraSource::Mjpeg { url, password, .. }
            | CameraSource::Snapshot { url, password, .. } => {
                *url = crate::redact::strip_userinfo(url);
                if password.is_some() {
                    *password = Some(crate::redact::MASK.into());
                }
            }
            CameraSource::Synthetic { .. } => {}
        }
        s
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionPolicy {
    /// Delete unlocked recordings older than this many days.
    pub max_days: Option<u32>,
    /// Delete oldest unlocked recordings while camera usage exceeds this.
    pub max_bytes: Option<u64>,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            max_days: Some(30),
            max_bytes: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordingConfig {
    /// Weekly schedule; defaults to continuous 24/7.
    pub schedule: Schedule,
    /// Segment length; a new file is started after this many seconds.
    pub segment_seconds: u32,
    /// Seconds of buffered frames written before a motion trigger.
    pub pre_record_seconds: u32,
    /// Seconds to keep recording after the last motion.
    pub post_record_seconds: u32,
    /// Frames per second written to disk (frames in between are dropped).
    pub max_fps: u32,
    pub retention: RetentionPolicy,
}

impl Default for RecordingConfig {
    fn default() -> Self {
        Self {
            schedule: Schedule::default(),
            segment_seconds: 300,
            pre_record_seconds: 5,
            post_record_seconds: 10,
            max_fps: 10,
            retention: RetentionPolicy::default(),
        }
    }
}

/// Rectangle in normalised coordinates (0.0..=1.0, origin top-left).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Region {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MotionConfig {
    pub enabled: bool,
    /// 1..=99. Higher = smaller per-pixel luma change counts as motion.
    pub sensitivity: u8,
    /// 1..=99. Percent of the analysed area that must change to trigger.
    pub threshold: u8,
    /// Areas to analyse; empty = whole frame.
    pub regions: Vec<Region>,
    /// Minimum seconds between two motion events for the same camera.
    pub cooldown_seconds: u32,
}

impl Default for MotionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sensitivity: 50,
            threshold: 2,
            regions: Vec::new(),
            cooldown_seconds: 10,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Camera {
    pub id: CameraId,
    pub name: String,
    pub enabled: bool,
    pub source: CameraSource,
    pub recording: RecordingConfig,
    pub motion: MotionConfig,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Payload for creating or fully replacing a camera.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CameraInput {
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub source: CameraSource,
    #[serde(default)]
    pub recording: RecordingConfig,
    #[serde(default)]
    pub motion: MotionConfig,
}

fn default_true() -> bool {
    true
}

impl CameraInput {
    pub fn validate(&self) -> crate::Result<()> {
        use crate::Error::Invalid;
        if self.name.trim().is_empty() || self.name.len() > 64 {
            return Err(Invalid("name must be 1..=64 characters".into()));
        }
        match &self.source {
            CameraSource::Rtsp { url, .. }
                if !url.starts_with("rtsp://") && !url.starts_with("rtsps://") =>
            {
                return Err(Invalid(
                    "rtsp url must start with rtsp:// or rtsps://".into(),
                ))
            }
            CameraSource::Mjpeg { url, .. } | CameraSource::Snapshot { url, .. }
                if !url.starts_with("http://") && !url.starts_with("https://") =>
            {
                return Err(Invalid("url must start with http:// or https://".into()))
            }
            CameraSource::Snapshot { interval_ms, .. } if *interval_ms < 50 => {
                return Err(Invalid("interval_ms must be >= 50".into()))
            }
            CameraSource::Synthetic { width, height, fps }
                if *width == 0
                    || *height == 0
                    || *width > 4096
                    || *height > 4096
                    || *fps == 0
                    || *fps > 60 =>
            {
                return Err(Invalid(
                    "synthetic size must be 1..=4096 and fps 1..=60".into(),
                ))
            }
            _ => {}
        }
        let r = &self.recording;
        if !(10..=3600).contains(&r.segment_seconds) {
            return Err(Invalid("segment_seconds must be 10..=3600".into()));
        }
        if r.pre_record_seconds > 30 || r.post_record_seconds > 600 {
            return Err(Invalid(
                "pre_record_seconds <= 30 and post_record_seconds <= 600".into(),
            ));
        }
        if !(1..=60).contains(&r.max_fps) {
            return Err(Invalid("max_fps must be 1..=60".into()));
        }
        let m = &self.motion;
        if !(1..=99).contains(&m.sensitivity) || !(1..=99).contains(&m.threshold) {
            return Err(Invalid("sensitivity and threshold must be 1..=99".into()));
        }
        for reg in &m.regions {
            let ok = (0.0..=1.0).contains(&reg.x)
                && (0.0..=1.0).contains(&reg.y)
                && reg.w > 0.0
                && reg.h > 0.0
                && reg.x + reg.w <= 1.0 + f32::EPSILON
                && reg.y + reg.h <= 1.0 + f32::EPSILON;
            if !ok {
                return Err(Invalid("regions must lie within the unit square".into()));
            }
        }
        Ok(())
    }
}

/// Runtime state of a camera worker (not persisted).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionState {
    Disabled,
    Connecting,
    Online,
    Offline,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CameraStatus {
    pub camera_id: CameraId,
    pub state: ConnectionState,
    pub recording: bool,
    pub recording_trigger: Option<RecordingTrigger>,
    pub motion_active: bool,
    pub fps: f32,
    pub last_frame_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
}

// ---------------------------------------------------------------- recordings

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingTrigger {
    Continuous,
    Motion,
    Manual,
}

impl RecordingTrigger {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Continuous => "continuous",
            Self::Motion => "motion",
            Self::Manual => "manual",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "continuous" => Some(Self::Continuous),
            "motion" => Some(Self::Motion),
            "manual" => Some(Self::Manual),
            _ => None,
        }
    }
}

/// One closed or in-progress segment file on disk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Recording {
    pub id: RecordingId,
    pub camera_id: CameraId,
    /// Path of the `.mjpg` data file, relative to the storage root.
    pub path: String,
    pub start: DateTime<Utc>,
    /// `None` while the segment is still being written.
    pub end: Option<DateTime<Utc>>,
    pub frames: u64,
    pub size_bytes: u64,
    pub trigger: RecordingTrigger,
    /// Locked recordings are never removed by retention.
    pub locked: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewRecording {
    pub camera_id: CameraId,
    pub path: String,
    pub start: DateTime<Utc>,
    pub trigger: RecordingTrigger,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RecordingQuery {
    pub camera_id: Option<CameraId>,
    /// Recordings overlapping [from, to).
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub trigger: Option<RecordingTrigger>,
    pub locked: Option<bool>,
    pub limit: Option<u32>,
    pub offset: Option<u32>,
}

// ---------------------------------------------------------------- events

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    Motion,
    ConnectionLost,
    ConnectionRestored,
    RecordingStarted,
    RecordingStopped,
    StorageFull,
}

impl EventKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Motion => "motion",
            Self::ConnectionLost => "connection_lost",
            Self::ConnectionRestored => "connection_restored",
            Self::RecordingStarted => "recording_started",
            Self::RecordingStopped => "recording_stopped",
            Self::StorageFull => "storage_full",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "motion" => Self::Motion,
            "connection_lost" => Self::ConnectionLost,
            "connection_restored" => Self::ConnectionRestored,
            "recording_started" => Self::RecordingStarted,
            "recording_stopped" => Self::RecordingStopped,
            "storage_full" => Self::StorageFull,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub id: EventId,
    pub camera_id: Option<CameraId>,
    pub kind: EventKind,
    pub at: DateTime<Utc>,
    /// Motion score 0.0..=1.0 (fraction of analysed area changed); 0 otherwise.
    pub score: f32,
    pub message: String,
    pub recording_id: Option<RecordingId>,
    pub acknowledged: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewEvent {
    pub camera_id: Option<CameraId>,
    pub kind: EventKind,
    pub at: DateTime<Utc>,
    pub score: f32,
    pub message: String,
    pub recording_id: Option<RecordingId>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EventQuery {
    pub camera_id: Option<CameraId>,
    pub kind: Option<EventKind>,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub acknowledged: Option<bool>,
    pub limit: Option<u32>,
    pub offset: Option<u32>,
}

// ---------------------------------------------------------------- users

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Live view, playback, acknowledge events.
    Viewer,
    /// Viewer + manage cameras and recordings.
    Manager,
    /// Everything, including users and system settings.
    Admin,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Viewer => "viewer",
            Self::Manager => "manager",
            Self::Admin => "admin",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "viewer" => Some(Self::Viewer),
            "manager" => Some(Self::Manager),
            "admin" => Some(Self::Admin),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct User {
    pub id: UserId,
    pub username: String,
    pub role: Role,
    /// PHC string (argon2id). Never serialised to API clients.
    #[serde(skip_serializing, default)]
    pub password_hash: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub token: String,
    pub user_id: UserId,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

// ---------------------------------------------------------------- settings

/// Global, admin-editable settings (stored as one JSON row).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    /// Stop recording (and raise StorageFull) when free space drops below this.
    pub min_free_bytes: u64,
    /// Session lifetime in hours.
    pub session_ttl_hours: u32,
    /// Seconds between retention sweeps.
    pub retention_interval_seconds: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            min_free_bytes: 1 << 30,
            session_ttl_hours: 24,
            retention_interval_seconds: 300,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> CameraInput {
        CameraInput {
            name: "Front door".into(),
            enabled: true,
            source: CameraSource::Synthetic {
                width: 640,
                height: 480,
                fps: 5,
            },
            recording: RecordingConfig::default(),
            motion: MotionConfig::default(),
        }
    }

    #[test]
    fn validate_accepts_defaults_and_rejects_bad_values() {
        assert!(input().validate().is_ok());
        let mut i = input();
        i.name = " ".into();
        assert!(i.validate().is_err());
        let mut i = input();
        i.source = CameraSource::Rtsp {
            url: "http://x".into(),
            username: None,
            password: None,
        };
        assert!(i.validate().is_err());
        let mut i = input();
        i.motion.regions.push(Region {
            x: 0.5,
            y: 0.5,
            w: 0.6,
            h: 0.1,
        });
        assert!(i.validate().is_err());
    }

    #[test]
    fn source_serde_is_tagged_and_redacts() {
        let s = CameraSource::Mjpeg {
            url: "http://cam".into(),
            username: Some("u".into()),
            password: Some("p".into()),
        };
        let v = serde_json::to_value(s.redacted()).unwrap();
        assert_eq!(v["type"], "mjpeg");
        assert_eq!(v["password"], "********");
        let s = CameraSource::Rtsp {
            url: "rtsp://a:b@cam/x".into(),
            username: None,
            password: None,
        };
        assert_eq!(
            serde_json::to_value(s.redacted()).unwrap()["url"],
            "rtsp://cam/x"
        );
    }

    #[test]
    fn user_hash_not_serialised() {
        let u = User {
            id: 1,
            username: "a".into(),
            role: Role::Admin,
            password_hash: "secret".into(),
            created_at: Utc::now(),
        };
        assert!(!serde_json::to_string(&u).unwrap().contains("secret"));
    }
}
