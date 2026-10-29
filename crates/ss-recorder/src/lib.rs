//! On-disk segments, record/no-record policy, pre-record buffer and retention.
//! See docs/SPEC.md §8.

pub mod disk;
pub mod policy;
pub mod prebuffer;
pub mod retention;
pub mod segment;

pub use policy::RecordPolicy;
pub use prebuffer::PreBuffer;
pub use segment::{IndexEntry, SegmentReader, SegmentSummary, SegmentWriter};
