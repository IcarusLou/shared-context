#![forbid(unsafe_code)]

//! Bounded, best-effort telemetry transport. The implementation deliberately has
//! no dependency on any Shared Context business crate.

mod client;
mod event;
mod wire;

pub use client::{EmitOutcome, default_endpoint, default_logs_root, emit_to};
pub use event::{Authorization, EntryPoint, Event, EventKind, Outcome, new_invocation_id};
pub use wire::{DecodeError, FRAME_MAGIC, FRAME_MAX_BYTES, FRAME_VERSION, FrameDecoder};
