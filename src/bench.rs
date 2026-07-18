//! Bench mode: headless free-model orchestration for repeatable benchmarks.
//!
//! Bench mode runs a scenario against real free-model agents (e.g. OpenCode
//! with `--model openrouter/z-ai/glm-5.2`) launched as subprocesses with an
//! explicit `--max-turns` budget. This is the mode that measures generation
//! speed -- Stub mode writes no real content, and Interactive mode is
//! tmux-attached and not CI-friendly.
//!
//! The orchestrator reuses the existing `testing::run_testing` runner with a
//! launcher_override that injects `--max-turns` and the free model id. The
//! squire's tick calls `cmd_wake`, which spawns the agent as a subprocess
//! (the same path Stub mode uses). The agent runs its bounded slice, calls
//! `trelane done` (or parks and exits), and the squire re-wakes it next tick
//! with a fresh budget -- persist+resume across slices via the existing
//! wake/done/park protocol, not a new one.
//!
//! The events file (`bench-events.jsonl`) records every new message and tick
//! so the live TUI (step 4) can tail it and keep the user informed. Without
//! the TUI, the events file is still a complete record of the run.
//!
//! The free-models-only filter prevents accidental paid-model spend: when
//! `bench.free_models` is non-empty and `--free-models-only` is passed, the
//! model is validated against the allowlist before any agent launches.

use crate::error::{Result, TrelaneError};
use crate::models::Config;
use rusqlite::{Connection, params};
use serde::Serialize;
use std::io::Write;
use std::path::Path;

/// Build the launcher override for bench mode: a headless opencode command
/// with `--max-turns` injected. The override replaces the per-agent launcher
/// for the duration of the bench run; all agents use the same model (V1;
/// per-agent models are a follow-up). The `{root}` and `{prompt_file}`
/// placeholders are substituted at launch time by `launcher_command_for_agent`.
pub fn build_launcher_override(model: &str, max_turns: u32) -> String {
    format!(
        "opencode run --model {model} --dir {{root}} --max-turns {max_turns} \
         --non-interactive \"$(cat {{prompt_file}})\""
    )
}

/// Validate that a model id is in the free-models allowlist. An empty list
/// means no restriction is configured (the operator has not set the
/// allowlist yet). A non-empty list that does not contain the model rejects
/// it with an error naming the offending model and the allowlist.
pub fn validate_free_model(config: &Config, model: &str) -> Result<()> {
    if config.bench.free_models.is_empty() {
        return Ok(());
    }
    if config.bench.free_models.iter().any(|m| m == model) {
        return Ok(());
    }
    Err(TrelaneError::msg(format!(
        "model '{model}' is not in bench.free_models {:?}. \
         A --free-models-only bench cannot use a paid model. Add it via \
         `trelane config set bench.free_models '{model}'` or drop \
         --free-models-only.",
        config.bench.free_models
    )))
}

/// A structured event written to `bench-events.jsonl`, one JSON object per
/// line. The live TUI (step 4) tails this file to show the user every
/// message and tick in real time. Without the TUI, the file is still a
/// complete record of the run.
#[derive(Debug, Clone, Serialize)]
pub struct BenchEvent {
    pub ts: String,
    pub kind: String,
    pub data: serde_json::Value,
}

/// Stateful writer for `bench-events.jsonl`. Tracks the max message rowid
/// seen so that `after_tick` can query only new messages since the last
/// tick -- one row per new message, plus one tick-summary row.
pub struct BenchEvents {
    file: std::fs::File,
    last_msg_rowid: i64,
}

impl BenchEvents {
    /// Create a new events file, truncating any prior content.
    pub fn create(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::File::create(path)?;
        Ok(Self {
            file,
            last_msg_rowid: 0,
        })
    }

    /// Emit events for one tick: every new message since the last call (by
    /// rowid), then a tick-summary event. Flushed immediately so the TUI can
    /// tail it live.
    pub fn after_tick(
        &mut self,
        conn: &Connection,
        tick: u32,
        launched: usize,
        running: usize,
    ) -> Result<()> {
        let mut stmt = conn.prepare(
            "SELECT rowid, id, from_agent, to_agent, msg_type, subject, created_at \
             FROM messages WHERE rowid > ?1 ORDER BY rowid",
        )?;
        let rows = stmt.query_map(params![self.last_msg_rowid], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
            ))
        })?;
        for row in rows {
            let (rowid, id, from, to, msg_type, subject, created_at) = row?;
            self.last_msg_rowid = rowid;
            let event = BenchEvent {
                ts: created_at,
                kind: "message_sent".to_string(),
                data: serde_json::json!({
                    "id": id,
                    "from": from,
                    "to": to,
                    "type": msg_type,
                    "subject": subject,
                }),
            };
            writeln!(self.file, "{}", serde_json::to_string(&event)?)?;
        }
        let tick_event = BenchEvent {
            ts: crate::crypto::now_iso(),
            kind: "tick".to_string(),
            data: serde_json::json!({
                "tick": tick,
                "launched": launched,
                "running": running,
            }),
        };
        writeln!(self.file, "{}", serde_json::to_string(&tick_event)?)?;
        self.file.flush()?;
        Ok(())
    }

    /// Emit a run-level event (start/end/error).
    pub fn emit(&mut self, kind: &str, data: serde_json::Value) -> Result<()> {
        let event = BenchEvent {
            ts: crate::crypto::now_iso(),
            kind: kind.to_string(),
            data,
        };
        writeln!(self.file, "{}", serde_json::to_string(&event)?)?;
        self.file.flush()?;
        Ok(())
    }
}

/// Run a bench scenario. Validates the free-model allowlist, builds the
/// launcher override with `--max-turns`, and delegates to the existing
/// `testing::run_testing` runner which handles sandbox setup, the step
/// loop, and the report file. The scenario's `mode` must be `"bench"`.
pub fn run_bench(
    scenario_path: &Path,
    runs: u32,
    report_path: Option<&Path>,
    sandbox_root: Option<&Path>,
    max_turns: Option<u32>,
    model: Option<&str>,
    free_models_only: bool,
    ui: bool,
) -> Result<()> {
    let config = crate::load_config()?;
    let max_turns = max_turns.unwrap_or(config.bench.default_max_turns);
    let model = model
        .or(config.bench.default_model.as_deref())
        .ok_or_else(|| {
            TrelaneError::msg(
                "bench run requires a model: pass --model <id> or set \
                 bench.default_model in config",
            )
        })?;

    if free_models_only {
        validate_free_model(&config, model)?;
    }

    // Validate the scenario mode is Bench. Without this, `trelane bench run`
    // on a Stub-mode scenario would silently run in Stub mode (short timeout,
    // no events file) -- the launcher_override would be set but the
    // orchestration wouldn't match. The scenario's mode determines whether
    // events are written and how long wait_for_idle waits; it must be Bench.
    let scenario_text = std::fs::read_to_string(scenario_path)?;
    let scenario: crate::testing::Scenario = serde_json::from_str(&scenario_text)?;
    if !matches!(scenario.mode, crate::testing::ScenarioMode::Bench) {
        return Err(TrelaneError::msg(format!(
            "trelane bench run requires \"mode\": \"bench\" in the scenario, but '{}' has \
             mode {:?}. Use `trelane --testing <file>` for Stub/Interactive scenarios, or \
             change the scenario's mode field to \"bench\".",
            scenario_path.display(),
            scenario.mode
        )));
    }

    let launcher_override = build_launcher_override(model, max_turns);
    eprintln!("[bench] model={model} max_turns={max_turns} free_models_only={free_models_only}");

    if ui {
        // The events file is created by run_once inside the sandbox. The TUI
        // tails it. The path follows the convention: sandbox_root/scenario-run-1/
        // bench-events.jsonl (run 1 is the first run; for multi-run benches,
        // the TUI shows the first run's events -- multi-run TUI is a follow-up).
        let sandbox = sandbox_root
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| std::env::temp_dir().join("trelane-testing"));
        let events_path = sandbox.join("scenario-run-1").join("bench-events.jsonl");

        let scenario_name = scenario.name.clone();
        let model_owned = model.to_string();
        let runs_val = runs;
        let scenario_path_owned = scenario_path.to_path_buf();
        let report_owned = report_path.map(std::path::Path::to_path_buf);
        let sandbox_owned = sandbox;

        crate::bench_ui::run_with_tui(
            &events_path,
            &scenario_name,
            &model_owned,
            max_turns,
            runs_val,
            move || {
                crate::testing::run_testing(
                    &scenario_path_owned,
                    runs_val,
                    report_owned.as_deref(),
                    Some(&sandbox_owned),
                    Some(&launcher_override),
                )
            },
        )
    } else {
        crate::testing::run_testing(
            scenario_path,
            runs,
            report_path,
            sandbox_root,
            Some(&launcher_override),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launcher_override_includes_max_turns_and_model() {
        let cmd = build_launcher_override("openrouter/z-ai/glm-5.2", 50);
        assert!(
            cmd.contains("--model openrouter/z-ai/glm-5.2"),
            "has the model: {cmd}"
        );
        assert!(cmd.contains("--max-turns 50"), "has max-turns: {cmd}");
        assert!(
            cmd.contains("--non-interactive"),
            "is non-interactive: {cmd}"
        );
        assert!(cmd.contains("{root}"), "has root placeholder: {cmd}");
        assert!(
            cmd.contains("{prompt_file}"),
            "has prompt_file placeholder: {cmd}"
        );
    }

    #[test]
    fn validate_free_model_passes_when_allowlist_empty() {
        let mut config = Config::default();
        config.bench.free_models = vec![];
        assert!(validate_free_model(&config, "any-paid-model").is_ok());
    }

    #[test]
    fn validate_free_model_passes_when_model_in_list() {
        let mut config = Config::default();
        config.bench.free_models = vec![
            "openrouter/z-ai/glm-5.2".to_string(),
            "free-model-x".to_string(),
        ];
        assert!(validate_free_model(&config, "openrouter/z-ai/glm-5.2").is_ok());
        assert!(validate_free_model(&config, "free-model-x").is_ok());
    }

    #[test]
    fn validate_free_model_rejects_paid_model() {
        let mut config = Config::default();
        config.bench.free_models = vec!["openrouter/z-ai/glm-5.2".to_string()];
        let err = validate_free_model(&config, "anthropic/claude-sonnet-4")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("anthropic/claude-sonnet-4"),
            "names the offending model: {err}"
        );
        assert!(
            err.contains("bench.free_models"),
            "names the allowlist key: {err}"
        );
    }

    #[test]
    fn bench_events_writes_message_and_tick_events() {
        let temp = tempfile::tempdir().unwrap();
        let events_path = temp.path().join("bench-events.jsonl");
        let ctx = crate::testing::bench_test_ctx(&temp);
        let mut events = BenchEvents::create(&events_path).unwrap();

        // Insert a message and then call after_tick -- should emit one
        // message_sent event and one tick event.
        let msg = crate::models::Message::new(
            "msg-test-1".to_string(),
            "alpha".to_string(),
            "beta".to_string(),
            "question".to_string(),
            "normal".to_string(),
            "test subject".to_string(),
            "test body".to_string(),
            None,
            None,
            vec![],
            crate::crypto::now_iso(),
        );
        crate::store::insert_message(&ctx.conn, &msg).unwrap();

        events.after_tick(&ctx.conn, 1, 2, 1).unwrap();

        let lines: Vec<String> = std::fs::read_to_string(&events_path)
            .unwrap()
            .lines()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(lines.len(), 2, "one message + one tick event");
        let msg_event: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(msg_event["kind"], "message_sent");
        assert_eq!(msg_event["data"]["from"], "alpha");
        assert_eq!(msg_event["data"]["to"], "beta");
        assert_eq!(msg_event["data"]["type"], "question");
        assert_eq!(msg_event["data"]["subject"], "test subject");
        let tick_event: serde_json::Value = serde_json::from_str(&lines[1]).unwrap();
        assert_eq!(tick_event["kind"], "tick");
        assert_eq!(tick_event["data"]["tick"], 1);
        assert_eq!(tick_event["data"]["launched"], 2);
        assert_eq!(tick_event["data"]["running"], 1);
    }

    #[test]
    fn bench_events_only_emits_new_messages_after_each_tick() {
        let temp = tempfile::tempdir().unwrap();
        let events_path = temp.path().join("bench-events.jsonl");
        let ctx = crate::testing::bench_test_ctx(&temp);
        let mut events = BenchEvents::create(&events_path).unwrap();

        // First tick: one message.
        let msg1 = crate::models::Message::new(
            "msg-1".to_string(),
            "a".to_string(),
            "b".to_string(),
            "info".to_string(),
            "normal".to_string(),
            "first".to_string(),
            "".to_string(),
            None,
            None,
            vec![],
            crate::crypto::now_iso(),
        );
        crate::store::insert_message(&ctx.conn, &msg1).unwrap();
        events.after_tick(&ctx.conn, 1, 1, 0).unwrap();

        // Second tick: a different message. The first must NOT be re-emitted.
        let msg2 = crate::models::Message::new(
            "msg-2".to_string(),
            "b".to_string(),
            "a".to_string(),
            "answer".to_string(),
            "normal".to_string(),
            "second".to_string(),
            "".to_string(),
            None,
            None,
            vec![],
            crate::crypto::now_iso(),
        );
        crate::store::insert_message(&ctx.conn, &msg2).unwrap();
        events.after_tick(&ctx.conn, 2, 0, 0).unwrap();

        let lines: Vec<String> = std::fs::read_to_string(&events_path)
            .unwrap()
            .lines()
            .map(|s| s.to_string())
            .collect();
        // tick 1: 1 message + 1 tick. tick 2: 1 message + 1 tick = 4 lines.
        assert_eq!(lines.len(), 4);
        let tick2_msg: serde_json::Value = serde_json::from_str(&lines[2]).unwrap();
        assert_eq!(
            tick2_msg["data"]["subject"], "second",
            "second tick emits only msg-2"
        );
    }
}
