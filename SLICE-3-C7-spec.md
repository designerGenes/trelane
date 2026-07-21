# SLICE-3 — C7 Observability & Completion: drop-in implementation spec

**Author lane:** docs/design (no `.rs` edited in producing this).
**Apply after:** SLICE-2 (C5). `telemetry.rs` and `squire.rs` edits are C7-exclusive and can land
anytime the tree is green; the `commands.rs` edits share that file with C5, so apply them **after**
C5 lands to avoid a merge collision.
**Verified against:** the current (SLICE-0-repaired) tree.

---

## 1. What already exists (do NOT rebuild)

- `telemetry.rs` is an OTLP-span system. `Tracer` writes spans to files; `compute_metrics(trace_dir)`
  aggregates them into `MetricsSummary { …, per_agent: Vec<AgentMetrics> }`. Existing span names:
  `agent.run`, `agent.wait`, `squire.tick`, `agent.rate`. Existing recorders: `record_agent_run`,
  `record_agent_wait`, `record_squire_tick`, `record_rating`. Helpers: `attr_str/attr_int/attr_bool`,
  `generate_span_id()`, `now_nanos()`, `iso_to_nanos(iso)`, `self.resource()`, `self.write_span()`.
- `record_squire_tick(agents_launched, agents_running, cycle_detected, start_ns, end_ns)` **exists but
  has no callers** — `squire::tick` emits no telemetry today.
- The completion **evaluator already exists**: `store::evaluate_project_completion`,
  `completion_fingerprint`, `record_completion_attestation`, `designate_project_role`,
  `upsert_validation_check`; `cmd_status` already renders completion. C7 only needs to *expose* it via
  CLI and *emit* the new metrics — not re-derive completion.
- Schema is at v10 (`project_roles`, `validation_checks`, `completion_attestations` present).
  **C7 needs no schema migration** — telemetry is file-based and the completion tables already exist.

---

## 2. Design principles

1. **Span-based, additive.** Emit new spans at protocol call sites (same pattern as `agent.run`);
   aggregate them in `compute_metrics`. No new DB tables.
2. **Best-effort telemetry.** Every emit is wrapped so a failure never aborts a tick or a protocol
   command: `if let Ok(tracer) = Tracer::ephemeral(&ctx.trelane_dir(), &ctx.root.display().to_string()) { let _ = tracer.record_…(); }`.
3. **Preserve `tick` signature.** `pub fn tick(ctx, launcher_override, verbose) -> Result<usize>` stays;
   add `tick_detailed` beside it and have `tick` delegate.

---

## 3. Metric catalog

| Metric (report name) | Definition | Source span / attribute |
|---|---|---|
| `help_offers_sent` | count of offers+requests | `assist.offer` |
| `help_offers_accepted` | count of accepts | `assist.accept` |
| `help_offer_response_latency` | accept_ts − offer_ts | `assist.accept` attrs `offer_ts`,`accept_ts` |
| `delegated_diff_rejection_rate` | (reject + request-changes) / all delegated reviews | `assist.review` attr `decision` |
| `owner_review_latency` | review_ts − submit_ts | `assist.review` attrs `submit_ts`,`review_ts` |
| `duplicate_assignment_attempts` | capacity-rejected accepts | `assist.capacity_rejected` |
| `cross_domain_violation_count` | violations recorded | `assist.violation` |
| `task_cycle_time_with_help` / `_without_help` | done_ts − ready_ts, split by `had_help` | `task.completed` attr `had_help` |
| `idle_with_ready_backlog_seconds` (proxy) | Σ `deferred_ready` over ticks (occurrences); seconds form needs tick interval — see §6 note | `squire.tick` attr `deferred_ready` |
| `agent_capacity_seconds` (proxy) | Σ `ready_backlog` over ticks | `squire.tick` attr `ready_backlog` |
| `assist_discovery_runs` | Σ `discovery_runs` over ticks | `squire.tick` attr `discovery_runs` |

---

## 4. `telemetry.rs` changes

### 4a. New `Tracer` recorders (mirror the existing `record_*` bodies exactly)

```rust
// span "assist.offer"  — attrs: assist.helper, assist.owner, assist.task, assist.kind ("offer"|"request")
pub fn record_assist_offer(&self, helper: &str, owner: &str, task_id: &str, kind: &str, ts_ns: u64) -> Result<SpanId>;

// span "assist.accept" — attrs: assist.helper, assist.owner, assist.task, assist.delegation,
//                                assist.offer_ts (int ns), assist.accept_ts (int ns)
pub fn record_assist_accept(&self, helper: &str, owner: &str, task_id: &str, delegation_id: &str,
                            offer_ts_ns: u64, accept_ts_ns: u64) -> Result<SpanId>;

// span "assist.submit" — attrs: assist.helper, assist.task, assist.delegation
pub fn record_assist_submit(&self, helper: &str, task_id: &str, delegation_id: &str, ts_ns: u64) -> Result<SpanId>;

// span "assist.review" — attrs: assist.reviewer, assist.task, assist.delegation,
//                                assist.decision ("accept"|"request-changes"|"reject"),
//                                assist.submit_ts (int ns), assist.review_ts (int ns)
pub fn record_assist_review(&self, reviewer: &str, task_id: &str, delegation_id: &str, decision: &str,
                            submit_ts_ns: u64, review_ts_ns: u64) -> Result<SpanId>;

// span "assist.capacity_rejected" — attrs: assist.helper, assist.owner, assist.task
pub fn record_capacity_rejection(&self, helper: &str, owner: &str, task_id: &str, ts_ns: u64) -> Result<SpanId>;

// span "assist.violation" — attrs: assist.agent, assist.paths (comma-joined)
pub fn record_cross_domain_violation(&self, agent: &str, paths: &[String], ts_ns: u64) -> Result<SpanId>;

// span "task.completed" — attrs: task.id, task.owner, task.had_help (bool)
//   span start=ready_ts_ns, end=done_ts_ns so duration is the cycle time.
pub fn record_task_completed(&self, task_id: &str, owner: &str, had_help: bool,
                             ready_ts_ns: u64, done_ts_ns: u64) -> Result<SpanId>;
```

Each body: `let span_id = generate_span_id();` → build `OtlpSpan { trace_id: self.trace_id.clone(),
span_id: span_id.clone(), parent_span_id: None, name: "<span>".into(), kind: 1, start_time_unix_nano,
end_time_unix_nano, attributes: vec![…], status: OtlpStatus{code:1,message:String::new()},
resource: self.resource() }` → `self.write_span(&span)?; Ok(span_id)`. For zero-duration events set
`start == end == ts_ns`.

### 4b. Extend `record_squire_tick`

Add three params (it has no callers, so the change is free):
```rust
pub fn record_squire_tick(&self, agents_launched: usize, agents_running: usize, cycle_detected: bool,
                          ready_backlog: usize, deferred_ready: usize, discovery_runs: usize,
                          start_ns: u64, end_ns: u64) -> Result<SpanId>;
```
Add attrs `squire.ready_backlog`, `squire.deferred_ready`, `squire.discovery_runs`.

### 4c. Extend `MetricsSummary` (add fields; keep existing)

```rust
pub help_offers_sent: usize,
pub help_offers_accepted: usize,
pub avg_help_offer_response_ms: f64,
pub delegated_reviews: usize,
pub delegated_rejections: usize,          // reject + request-changes
pub delegated_diff_rejection_rate: f64,   // delegated_rejections / delegated_reviews
pub avg_owner_review_latency_ms: f64,
pub duplicate_assignment_attempts: usize,
pub cross_domain_violations: usize,
pub tasks_completed_with_help: usize,
pub tasks_completed_without_help: usize,
pub avg_task_cycle_ms_with_help: f64,
pub avg_task_cycle_ms_without_help: f64,
pub total_ready_backlog: usize,
pub total_deferred_ready: usize,          // idle-with-ready-backlog occurrences
pub total_discovery_runs: usize,
```
(Optionally mirror the per-agent ones onto `AgentMetrics` keyed by helper/owner — not required for a first cut.)

### 4d. Extend `compute_metrics`

Add match arms on `span.name`:
- `"assist.offer"` → `help_offers_sent += 1`.
- `"assist.accept"` → `help_offers_accepted += 1`; accumulate `accept_ts − offer_ts` for the avg.
- `"assist.review"` → `delegated_reviews += 1`; if decision != "accept" → `delegated_rejections += 1`;
  accumulate `review_ts − submit_ts`.
- `"assist.capacity_rejected"` → `duplicate_assignment_attempts += 1`.
- `"assist.violation"` → `cross_domain_violations += 1`.
- `"task.completed"` → bucket `(end − start)` by `had_help`.
- `"squire.tick"` → add `ready_backlog`, `deferred_ready`, `discovery_runs` to their totals.
Compute the `avg_*` and `*_rate` fields at the end guarding divide-by-zero (return `0.0` when denom 0).

---

## 5. `squire.rs` changes

```rust
pub struct TickOutcome {
    pub launched: usize,
    pub concurrency: usize,     // running_count + launched
    pub ready_backlog: usize,   // plan.candidates.len()
    pub deferred_ready: usize,  // report.deferred
    pub discovery_runs: usize,  // AssistDiscovery candidates actually launched
}

pub fn tick_detailed(ctx: &Context, launcher_override: Option<&str>, verbose: bool) -> Result<TickOutcome> {
    // body = current tick() body, with:
    //   let start_ns = telemetry::now_nanos();  (before reap_leases)
    //   let ready_backlog = plan.candidates.len();
    //   track `discovery_runs` (increment inside Ok(()) when cand.kind == WakeKind::AssistDiscovery)
    //   after the loop, best-effort emit:
    //     let end_ns = telemetry::now_nanos();
    //     if let Ok(t) = telemetry::Tracer::ephemeral(&ctx.trelane_dir(), &ctx.root.display().to_string()) {
    //         let _ = t.record_squire_tick(launched, running_count, plan.cycle.is_some(),
    //                                      ready_backlog, report.deferred, discovery_runs, start_ns, end_ns);
    //     }
    //   return Ok(TickOutcome { launched, concurrency: running_count + launched,
    //                           ready_backlog, deferred_ready: report.deferred, discovery_runs });
    // NOTE: early-return path (plan.candidates.is_empty()) returns TickOutcome::default()-ish (all zero).
}

pub fn tick(ctx: &Context, launcher_override: Option<&str>, verbose: bool) -> Result<usize> {
    Ok(tick_detailed(ctx, launcher_override, verbose)?.launched)
}
```
`report.deferred` and `report.budget` already exist on the `concurrency_report` result used in `tick`.

---

## 6. Call-site map (exact hooks; all best-effort)

| Emit | File · fn (current line) | Data in scope |
|---|---|---|
| `assist.offer` (kind=`request`) | `commands.rs` · `cmd_help_request` (1909) | helper, owner, task |
| `assist.offer` (kind=`offer`) | `commands.rs` · `cmd_help_offer` (1951) | helper, owner, task |
| `assist.accept` | `commands.rs` · `cmd_help_accept` (2035), **after** `activate_delegation_and_assign` succeeds | `offered.helper_agent`, `owner`, `offered.task_id`, `id`, `offer_ts = iso_to_nanos(&offered.issued_at)`, `accept_ts = iso_to_nanos(&now)` |
| `assist.capacity_rejected` | `commands.rs` · `cmd_help_accept` (2035), in the **Err** path of `activate_delegation_and_assign` when the error is the capacity rejection from `store.rs:1175` (`active_helpers >= desired_parallelism`) | same identifiers; emit then propagate the error |
| `assist.submit` | `commands.rs` · `cmd_work_submit` (2648), after the submission row is recorded | helper, task, delegation |
| `assist.review` | `commands.rs` · `cmd_work_review` (2760), after the review is recorded; fetch submit ts via `store::latest_submission_for_delegation(delegation_id)?.created_at` | reviewer, task_id, delegation_id, `decision.as_str()`, submit_ts, review_ts=now |
| `assist.violation` | wherever `store::insert_violation` is called (audit path) | agent, paths |
| `task.completed` | the command that transitions a task to `Done` (the `work`/done flow calling `store::set_task_state(.., Done ..)`) | task id, owner, `had_help = !store::list_delegations_for_task(id)?.is_empty()`, `ready_ts` (see note), `done_ts = now` |

**capacity-rejection detection:** simplest is to give `store::activate_delegation_and_assign` a
distinguishable error (e.g. a `TrelaneError` variant/kind `CapacityExceeded`) so `cmd_help_accept`
can match it. If you'd rather not touch the error type, pre-check in `cmd_help_accept` with
`store::list_assignments_for_task` vs `task.desired_parallelism` and emit before calling activate.

**`ready_ts` note:** the ledger stores only `created_at`/`updated_at` per task, not a per-transition
`ready_at`. First cut: use `task.created_at` as `ready_ts` (documented approximation). Exact cycle
time would need a `task.ready` span emitted on the `ready` transition — optional follow-up.

---

## 7. Optional: completion CLI (`trelane project …`)

Everything below already exists in `store`; this only surfaces it.

- `cli.rs`: add `Project { #[command(subcommand)] action: ProjectAction }` to `Command`, and
  `pub enum ProjectAction { Status { #[arg(long)] json: bool }, Designate { agent, role, by },
  Attest { by, role, #[arg(long)] note: Option<String> }, Check { name, #[arg(long)] status: String } }`.
- `lib.rs`: add the `Some(Command::Project { action }) => { let ctx = Context::open(...)?; commands::cmd_project(&ctx, &action) }` arm.
- `commands.rs`: `cmd_project` dispatch →
  `Status` = print `store::evaluate_project_completion` (+ `completion_fingerprint`);
  `Designate` = `store::designate_project_role`; `Attest` = `store::record_completion_attestation`
  (guard: attester must hold a designated role — evaluator/status already model this);
  `Check` = `store::upsert_validation_check`.

---

## 8. Test matrix

| Test | Assertion |
|---|---|
| `idle_with_ready_backlog_counted` | when `candidates > budget`, `tick_detailed().deferred_ready > 0` and the `squire.tick` span carries it |
| `telemetry_failure_does_not_fail_tick` | with an unwritable trace dir, `tick`/`tick_detailed` still return `Ok(..)` |
| `tick_delegates_to_detailed` | `tick(..)? == tick_detailed(..)?.launched` |
| `offer_then_accept_metrics` | offer→accept produces `help_offers_sent==1`, `help_offers_accepted==1`, `avg_help_offer_response_ms >= 0` |
| `delegated_reject_rate` | one accept-review + one reject-review → `delegated_diff_rejection_rate == 0.5` |
| `capacity_rejection_metric` | accepting beyond `desired_parallelism` → `duplicate_assignment_attempts == 1` and the accept fails |
| `review_latency_positive` | `avg_owner_review_latency_ms > 0` when submit precedes review |
| `metrics_json_surfaces_new_fields` | `trelane metrics --json` includes the new keys |
| `project_status_cli` | `trelane project status` matches `evaluate_project_completion` |

---

## 9. Cross-cutting rules (must hold)

- Telemetry emit is **always** `if let Ok(tracer) … { let _ = tracer.record_…(); }`; never `?` it in a
  hot path. A telemetry failure must not abort a tick or a protocol command.
- Keep `tick`'s public signature `-> Result<usize>`.
- No schema migration for C7. If SLICE-4 later adds one, it takes `user_version = 11`.
- These edits touch `commands.rs`, which C5 also edits — **apply after C5** and rebuild between edits.
  `telemetry.rs`/`squire.rs`/`cli.rs`(Project) are C7-exclusive and safe to land first.
