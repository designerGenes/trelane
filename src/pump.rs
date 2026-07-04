//! Backwards-compatibility shim.
//!
//! The relauncher formerly known as "the pump" (and briefly "the prop")
//! is now "the squire" -- Trelane's dutiful, tireless assistant who
//! restarts agents and keeps the workflow in motion.  This shim exists
//! so external code and older scripts that referenced `trelane::pump::*`
//! or `trelane::prop::*` keep compiling.  New code should use
//! `crate::squire`.

pub use crate::squire::*;
