//! Runtime pieces that wire the core contracts together.
//!
//! The engine stays platform-agnostic: it owns a source, digital VFO channels,
//! and sinks. Audio devices and egui live in the application crate.

pub mod recorder;
pub mod session;
pub mod vfo;

pub use recorder::{RecorderHandle, RecorderSink, RecordingInfo};
pub use session::LiveSession;
pub use vfo::VfoChannel;
