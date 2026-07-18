//! Shared row-based config editor primitives.
//!
//! Extracted verbatim from `diagnostic.rs` so both the standalone
//! `trelane diagnostic` view AND the embedded editor in the Trelane-monitor's
//! diagnostic tab operate on ONE definition of "which config keys are
//! editable and how". `diagnostic.rs` re-exports these and delegates to them,
//! so its behavior is unchanged; `monitor.rs` uses them directly. Any future
//! config key becomes editable in both places by editing only this file.
//!
//! Pure: no I/O. `fields_from_config` reads a Config into editable rows;
//! `apply_fields_to_config` writes edited rows back onto a Config. The two are
//! inverses over the keys they cover; keys they don't cover are left untouched.

use crate::models::Config;

/// The kind of value an editable config field holds, which determines how
/// keypresses mutate it.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldValue {
    Bool(bool),
    /// Unsigned integer with an inclusive [min, max] clamp and a step.
    Uint {
        value: u64,
        min: u64,
        max: u64,
        step: u64,
    },
    /// Optional unsigned integer (None renders as "off"); toggling from None
    /// yields `default_on`.
    OptUint {
        value: Option<u64>,
        default_on: u64,
        min: u64,
        max: u64,
        step: u64,
    },
}

/// A single editable row in the Config tab.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigField {
    pub key: &'static str,
    pub label: &'static str,
    pub value: FieldValue,
}

impl ConfigField {
    /// A short human-readable rendering of the current value.
    pub fn display_value(&self) -> String {
        match &self.value {
            FieldValue::Bool(b) => (if *b { "[x]" } else { "[ ]" }).to_string(),
            FieldValue::Uint { value, .. } => value.to_string(),
            FieldValue::OptUint { value, .. } => value
                .map(|v| v.to_string())
                .unwrap_or_else(|| "off".to_string()),
        }
    }

    /// Apply a leftward/decrement or rightward/increment adjustment.
    pub fn adjust(&mut self, increase: bool) {
        match &mut self.value {
            FieldValue::Bool(b) => *b = !*b,
            FieldValue::Uint {
                value,
                min,
                max,
                step,
            } => {
                *value = if increase {
                    (*value).saturating_add(*step).min(*max)
                } else {
                    (*value).saturating_sub(*step).max(*min)
                };
            }
            FieldValue::OptUint {
                value,
                default_on,
                min,
                max,
                step,
            } => {
                match value {
                    None => {
                        if increase {
                            *value = Some((*default_on).clamp(*min, *max));
                        }
                        // decrement from "off" stays off
                    }
                    Some(v) => {
                        if increase {
                            *v = (*v).saturating_add(*step).min(*max);
                        } else if *v <= *min {
                            *value = None; // stepping below min turns it off
                        } else {
                            *v = (*v).saturating_sub(*step).max(*min);
                        }
                    }
                }
            }
        }
    }

    /// Toggle: meaningful for Bool and OptUint (on/off); flips Uint between
    /// min and max as a coarse convenience.
    pub fn toggle(&mut self) {
        match &mut self.value {
            FieldValue::Bool(b) => *b = !*b,
            FieldValue::OptUint {
                value,
                default_on,
                min,
                max,
                ..
            } => {
                *value = match value {
                    Some(_) => None,
                    None => Some((*default_on).clamp(*min, *max)),
                };
            }
            FieldValue::Uint {
                value, min, max, ..
            } => {
                *value = if *value == *max { *min } else { *max };
            }
        }
    }
}

/// Build the editable field list from a Config. Single source of truth for the
/// config<->fields mapping; `apply_fields_to_config` is its inverse.
pub fn fields_from_config(config: &Config) -> Vec<ConfigField> {
    vec![
        ConfigField {
            key: "squire.interval_s",
            label: "Squire tick interval (s)",
            value: FieldValue::Uint {
                value: config.squire.interval_s,
                min: 1,
                max: 3600,
                step: 1,
            },
        },
        ConfigField {
            key: "squire.max_concurrent",
            label: "Max concurrent agents",
            value: FieldValue::Uint {
                value: config.squire.max_concurrent as u64,
                min: 1,
                max: 64,
                step: 1,
            },
        },
        ConfigField {
            key: "squire.reply_timeout_s",
            label: "Reply-wait timeout (s)",
            value: FieldValue::OptUint {
                value: config.squire.reply_timeout_s,
                default_on: 3600,
                min: 30,
                max: 86_400,
                step: 30,
            },
        },
        ConfigField {
            key: "squire.breaker_escalation_count",
            label: "Breaker escalation count",
            value: FieldValue::Uint {
                value: config.squire.breaker_escalation_count as u64,
                min: 1,
                max: 100,
                step: 1,
            },
        },
        ConfigField {
            key: "squire.starvation_ticks",
            label: "Starvation guarantee (ticks)",
            value: FieldValue::Uint {
                value: config.squire.starvation_ticks as u64,
                min: 1,
                max: 10_000,
                step: 1,
            },
        },
        ConfigField {
            key: "di.objection_window_s",
            label: "DI objection window (s)",
            value: FieldValue::Uint {
                value: config.di.objection_window_s,
                min: 0,
                max: 86_400,
                step: 30,
            },
        },
        ConfigField {
            key: "di.request_timeout_s",
            label: "DI request timeout (s)",
            value: FieldValue::Uint {
                value: config.di.request_timeout_s,
                min: 60,
                max: 604_800,
                step: 60,
            },
        },
        ConfigField {
            key: "di.claim_contested_timeout_s",
            label: "DI claim-contested timeout (s)",
            value: FieldValue::Uint {
                value: config.di.claim_contested_timeout_s,
                min: 60,
                max: 604_800,
                step: 60,
            },
        },
        ConfigField {
            key: "retention.hot_days",
            label: "Retention hot window (days)",
            value: FieldValue::Uint {
                value: config.retention.hot_days,
                min: 1,
                max: 3650,
                step: 1,
            },
        },
        ConfigField {
            key: "retention.dormant_days",
            label: "Project dormant window (days)",
            value: FieldValue::Uint {
                value: config.retention.dormant_days,
                min: 1,
                max: 3650,
                step: 1,
            },
        },
        ConfigField {
            key: "retention.purge_days",
            label: "Retention purge (days)",
            value: FieldValue::OptUint {
                value: config.retention.purge_days,
                default_on: 365,
                min: 1,
                max: 3650,
                step: 1,
            },
        },
        ConfigField {
            key: "claims.default_ttl_s",
            label: "Claim TTL (s)",
            value: FieldValue::Uint {
                value: config.claims.default_ttl_s,
                min: 30,
                max: 86_400,
                step: 30,
            },
        },
        ConfigField {
            key: "biplane.detect_thematic_deadlock",
            label: "Detect thematic deadlock",
            value: FieldValue::Bool(config.biplane.detect_thematic_deadlock),
        },
        ConfigField {
            key: "biplane.reanalyze_on_all_stop",
            label: "Reanalyze on all-stop",
            value: FieldValue::Bool(config.biplane.reanalyze_on_all_stop),
        },
        ConfigField {
            key: "ui.pane_navigation",
            label: "Pane navigation keys",
            value: FieldValue::Bool(config.ui.pane_navigation),
        },
        ConfigField {
            key: "ui.match_host_terminal",
            label: "Match host terminal keys",
            value: FieldValue::Bool(config.ui.match_host_terminal),
        },
    ]
}

/// Write the current field values back onto a Config. Inverse of
/// `fields_from_config`. Unknown keys are ignored defensively.

/// Write edited field values back onto a Config. Inverse of
/// `fields_from_config`. Unknown keys are ignored defensively.
pub fn apply_fields_to_config(fields: &[ConfigField], config: &mut Config) {
    for f in fields {
        match (f.key, &f.value) {
            ("squire.interval_s", FieldValue::Uint { value, .. }) => {
                config.squire.interval_s = *value
            }
            ("squire.max_concurrent", FieldValue::Uint { value, .. }) => {
                config.squire.max_concurrent = *value as usize
            }
            ("squire.reply_timeout_s", FieldValue::OptUint { value, .. }) => {
                config.squire.reply_timeout_s = *value
            }
            ("squire.breaker_escalation_count", FieldValue::Uint { value, .. }) => {
                config.squire.breaker_escalation_count = *value as i64
            }
            ("squire.starvation_ticks", FieldValue::Uint { value, .. }) => {
                config.squire.starvation_ticks = *value as i64
            }
            ("di.objection_window_s", FieldValue::Uint { value, .. }) => {
                config.di.objection_window_s = *value
            }
            ("di.request_timeout_s", FieldValue::Uint { value, .. }) => {
                config.di.request_timeout_s = *value
            }
            ("di.claim_contested_timeout_s", FieldValue::Uint { value, .. }) => {
                config.di.claim_contested_timeout_s = *value
            }
            ("retention.hot_days", FieldValue::Uint { value, .. }) => {
                config.retention.hot_days = *value
            }
            ("retention.dormant_days", FieldValue::Uint { value, .. }) => {
                config.retention.dormant_days = *value
            }
            ("retention.purge_days", FieldValue::OptUint { value, .. }) => {
                config.retention.purge_days = *value
            }
            ("claims.default_ttl_s", FieldValue::Uint { value, .. }) => {
                config.claims.default_ttl_s = *value
            }
            ("biplane.detect_thematic_deadlock", FieldValue::Bool(b)) => {
                config.biplane.detect_thematic_deadlock = *b
            }
            ("biplane.reanalyze_on_all_stop", FieldValue::Bool(b)) => {
                config.biplane.reanalyze_on_all_stop = *b
            }
            ("ui.pane_navigation", FieldValue::Bool(b)) => config.ui.pane_navigation = *b,
            ("ui.match_host_terminal", FieldValue::Bool(b)) => {
                config.ui.match_host_terminal = *b
            }
            _ => {}
        }
    }
}
