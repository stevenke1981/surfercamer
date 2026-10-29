//! Shared domain model and cross-crate contracts for surveillance-rs.
//!
//! Every other crate depends on this one. Types here are the single source of
//! truth for the REST API (serde), the database (ss-db) and the pipeline
//! (ss-camera → ss-motion → ss-recorder, wired by ss-engine).

pub mod error;
pub mod frame;
pub mod model;
pub mod redact;
pub mod schedule;
pub mod store;

pub use error::{Error, Result};
pub use frame::{Frame, FrameHub};
pub use model::*;
pub use schedule::{Schedule, ScheduleMode, SLOTS_PER_DAY, SLOTS_PER_WEEK};
pub use store::Store;
