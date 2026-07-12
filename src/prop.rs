//! Backwards-compatibility shim.
//!
//! The scheduler formerly known as "the prop" (and before that "the pump")
//! is now `crate::squire` -- Trelane's dutiful squire. This module used to
//! hold its own copy of the scheduler; that copy has been collapsed onto the
//! single source of truth in `crate::squire` so the two can never drift
//! (and so the config field they read stays `squire.max_concurrent`).
//!
//! This shim exists so external code and older scripts that referenced
//! `trelane::prop::*` keep compiling. New code should use `crate::squire`.

pub use crate::squire::*;
