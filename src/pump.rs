//! Backwards-compatibility shim.
//!
//! The relauncher formerly known as "the pump" is now "the prop" -- the
//! propeller that keeps the Trelane biplane flying. It is still dumb, it
//! still just spins on a timer, and it is still the only thing allowed to
//! relaunch agents. See `crate::prop` for the real module.
//!
//! This shim exists so external code and older scripts that referenced
//! `trelane::pump::*` keep compiling. New code should use `crate::prop`.

pub use crate::prop::*;
