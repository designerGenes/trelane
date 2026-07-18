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
//!
//! # Liveness boundary (hard rule, R3)
//!
//! **Trelane does not restart the squire itself.** Keeping the squire alive
//! is the host supervisor's job (launchd / systemd / cron / a `while true`
//! shell loop). Attempting it inside Trelane would create the exact
//! recursive-restarter problem R3 exists to prevent: a dumb restarter that
//! can also get stuck needs another dumb restarter above it, forever. The
//! squire is deliberately the *bottom* of that stack.

pub use crate::squire::*;
