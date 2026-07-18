//! Slice 5 (GAP-10): Biplane progressive domain refinement and adjacency.
//!
//! R17 -- refinement only ever ADDS precision: a pass may split a domain
//! into finer children; it never merges domains or silently reassigns
//! coverage. R18 -- granularity is an open-ended ladder (coarse ->
//! file-group -> feature), climbed one rung at a time per domain, driven by
//! that domain's own growth signal. R19 -- the refinement DECISION is model
//! judgment, but it runs only on deliberate, explicit invocation
//! (`trelane biplane --refine`), never on the squire's wake path. R20 -- a
//! split against an OWNED domain produces a proposal reviewed by the owner
//! (`trelane split ...`), never a fact; ownerless (disabled) domains split
//! freely. R21/R22 -- every domain carries a ranked adjacency list of what
//! to try next; moving still obeys ownership. R29 -- a pending proposal
//! surfaces at the owner's next wake as information, not a block.
//!
//! The mechanical core (leaf detection, growth signals, tier rules, split
//! application, sibling adjacency) is pure/deterministic and unit-tested
//! with a stub decider; the model only ever answers "split this leaf, yes
//! or no, and how" and "rank these cross-branch moves".

use crate::Context;
use crate::error::{Result, TrelaneError};
use crate::models::{Domain, SplitProposal, next_tier, tier_rank};
use crate::store;
use rusqlite::Connection;

/// A leaf domain: registered, still has writable coverage, and not the
/// parent of a finer domain.
#[derive(Debug, Clone)]
pub struct LeafDomain {
    pub name: String,
    pub writable: Vec<String>,
    pub tier: String,
}

/// Growth signal for one leaf, scoped to just that domain's files since its
/// tier was last set -- never global repo stats (R18).
#[derive(Debug, Clone, Default)]
pub struct GrowthSignal {
    pub domain: String,
    pub tier: String,
    pub file_count: usize,
    pub commits_since_tier: usize,
    /// A few example paths, so the decider sees shape, not just counts.
    pub sample_files: Vec<String>,
}

/// The model's answer for one leaf.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SplitDecision {
    #[serde(default)]
    pub split: bool,
    #[serde(default)]
    pub rationale: String,
    #[serde(default)]
    pub children: Vec<SplitChild>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SplitChild {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub writable: Vec<String>,
}

/// How a refinement decision is made. Production uses the LLM decider;
/// tests use a stub. This is the only place model judgment enters (R19).
pub trait SplitDecider {
    fn decide(&self, leaf: &LeafDomain, growth: &GrowthSignal) -> Result<SplitDecision>;
}

// ------------------------------------------------------------- leaf + growth

/// Current leaf domains, in deterministic (name) order.
pub fn leaf_domains(conn: &Connection) -> Result<Vec<LeafDomain>> {
    let mut leaves = Vec::new();
    for agent in store::list_agents(conn)? {
        let dom = match store::get_domain(conn, &agent)? {
            Some(d) => d,
            None => continue,
        };
        if dom.writable.is_empty() {
            continue; // retired (split) parent: lineage, not a leaf
        }
        let is_parent = store::list_agents(conn)?
            .iter()
            .filter(|a| a.as_str() != agent.as_str())
            .filter_map(|a| store::get_domain(conn, a).ok().flatten())
            .any(|other| other.parent_domain.as_deref() == Some(agent.as_str()));
        if is_parent {
            continue;
        }
        leaves.push(LeafDomain {
            name: agent,
            writable: dom.writable,
            tier: dom.granularity_tier,
        });
    }
    leaves.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(leaves)
}

/// Gather a leaf's growth signal: files matching its writable globs now,
/// plus commits touching those files since its tier was last set.
pub fn gather_growth(ctx: &Context, leaf: &LeafDomain, dom: &Domain) -> Result<GrowthSignal> {
    let compiled = crate::domain::CompiledDomain::from_domain(dom)?;
    let mut files = Vec::new();
    collect_domain_files(&ctx.root, &ctx.root.clone(), &compiled, &mut files, 0);
    files.sort();

    let since = dom
        .tier_set_at
        .clone()
        .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string());
    let commits_since_tier = count_domain_commits(&ctx.root, &since, &compiled, &files);

    Ok(GrowthSignal {
        domain: leaf.name.clone(),
        tier: leaf.tier.clone(),
        file_count: files.len(),
        commits_since_tier,
        sample_files: files.iter().take(12).cloned().collect(),
    })
}

fn collect_domain_files(
    root: &std::path::Path,
    dir: &std::path::Path,
    compiled: &crate::domain::CompiledDomain,
    out: &mut Vec<String>,
    depth: usize,
) {
    if depth > 8 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.')
            || matches!(
                name.as_str(),
                "target" | "node_modules" | "__pycache__" | "dist" | "build"
            )
        {
            continue;
        }
        if path.is_dir() {
            collect_domain_files(root, &path, compiled, out, depth + 1);
        } else if let Ok(rel) = path.strip_prefix(root) {
            let rel = rel.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/");
            if compiled.is_writable(&rel) {
                out.push(rel);
            }
        }
    }
}

fn count_domain_commits(
    root: &std::path::Path,
    since_iso: &str,
    _compiled: &crate::domain::CompiledDomain,
    current_files: &[String],
) -> usize {
    // Count commits since the tier was set that touched any of the domain's
    // current files. Best-effort: no git -> 0 (file count still signals).
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["log", "--since", since_iso, "--pretty=format:%H", "--"])
        .args(current_files)
        .output();
    match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter(|l| !l.trim().is_empty())
            .count(),
        _ => 0,
    }
}

// --------------------------------------------------------- LLM decider (R19)

/// The production decider: one focused model call per leaf, answering only
/// "split one tier deeper, yes or no, and into what". Runs exclusively on
/// the explicit `--refine` invocation -- never on a wake path (R19).
pub struct LlmSplitDecider<'a> {
    pub ctx: &'a Context,
    pub model: &'a str,
}

impl SplitDecider for LlmSplitDecider<'_> {
    fn decide(&self, leaf: &LeafDomain, growth: &GrowthSignal) -> Result<SplitDecision> {
        let prompt = format!(
            "You are Biplane, the planning layer of a Trelane swarm. Decide whether ONE domain \
             should be refined ONE granularity tier deeper ({} -> next tier). Refinement only \
             ever adds precision: a split partitions the domain's coverage into finer child \
             domains. Never merge, never reassign coverage elsewhere.\n\n\
             Domain: {}\nCurrent tier: {}\nWritable globs: {}\nFiles in domain: {}\n\
             Commits touching those files since the tier was set: {}\nSample files:\n{}\n\n\
             Split only if the domain has genuinely outgrown its tier (many files, active \
             churn, and a natural sub-structure). A quiet or small domain stays put.\n\n\
             Reply with STRICT JSON only, no prose, exactly this shape:\n\
             {{\"split\": true|false, \"rationale\": \"...\", \"children\": [\
             {{\"name\": \"child-name\", \"description\": \"...\", \"writable\": [\"glob/**\"]}}]}}\n\
             Rules: at most 4 children; every child writable glob must be INSIDE the parent's \
             coverage; child names are bare identifiers (they become '<parent>-<name>').",
            leaf.tier,
            leaf.name,
            leaf.tier,
            leaf.writable.join(", "),
            growth.file_count,
            growth.commits_since_tier,
            growth
                .sample_files
                .iter()
                .map(|f| format!("  - {f}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let output = run_model_once(self.ctx, self.model, "refine", &prompt)?;
        parse_json_object(&output)
    }
}

/// Run one model invocation via the configured launcher and return stdout.
fn run_model_once(ctx: &Context, model: &str, tag: &str, prompt: &str) -> Result<String> {
    let prompt_file = ctx
        .trelane_dir()
        .join(format!("refine-{tag}-prompt.md"));
    std::fs::write(&prompt_file, prompt)?;
    let cmd = crate::biplane::resolve_launcher_template(model)?
        .replace("{prompt_file}", &prompt_file.display().to_string())
        .replace("{agent}", "biplane-refine")
        .replace("{root}", &ctx.root.display().to_string());
    let runner = ctx.trelane_dir().join(format!("refine-{tag}-runner.sh"));
    std::fs::write(&runner, format!("#!/bin/sh\n{cmd}\n"))?;
    let out = std::process::Command::new("sh")
        .arg(&runner)
        .current_dir(&ctx.root)
        .stdin(std::process::Stdio::null())
        .output()?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let text = crate::biplane::extract_text_from_json_events(&stdout);
    if text.trim().is_empty() {
        Ok(stdout)
    } else {
        Ok(text)
    }
}

/// Extract and parse the first {...} JSON object in model output.
fn parse_json_object<T: serde::de::DeserializeOwned>(text: &str) -> Result<T> {
    let start = text.find('{');
    let end = text.rfind('}');
    match (start, end) {
        (Some(s), Some(e)) if e > s => serde_json::from_str(&text[s..=e]).map_err(|err| {
            TrelaneError::msg(format!("model returned invalid JSON: {err}"))
        }),
        _ => Err(TrelaneError::msg("model returned no JSON object")),
    }
}

// ----------------------------------------------------------- split mechanics

/// Validate proposed children against R17: each child's coverage must be a
/// subset of the parent's, children must not overlap each other, and names
/// must not collide with existing domains.
fn validate_children(
    conn: &Connection,
    parent: &Domain,
    children: &[SplitChild],
) -> Result<()> {
    if children.is_empty() || children.len() > 4 {
        return Err(TrelaneError::msg(
            "a split must produce between 1 and 4 children",
        ));
    }
    let mut names = std::collections::HashSet::new();
    for child in children {
        let full = child_full_name(&parent.agent, &child.name);
        if !names.insert(full.clone()) || store::agent_exists(conn, &full)? {
            return Err(TrelaneError::msg(format!(
                "child domain name '{full}' is duplicate or already registered"
            )));
        }
        if child.writable.is_empty() {
            return Err(TrelaneError::msg(format!(
                "child '{full}' has no writable globs"
            )));
        }
        for glob in &child.writable {
            if !crate::domain::domain_allows_scope(parent, glob)? {
                return Err(TrelaneError::msg(format!(
                    "child glob '{glob}' escapes parent domain '{}'s coverage (R17: \
                     a split only adds precision)",
                    parent.agent
                )));
            }
        }
        // Children must partition, not overlap, the parent's coverage.
        for other in children {
            if other.name == child.name {
                continue;
            }
            for a in &child.writable {
                for b in &other.writable {
                    if crate::domain::scope_entries_may_overlap(a, b)? {
                        return Err(TrelaneError::msg(format!(
                            "child globs '{a}' and '{b}' overlap (a split partitions coverage)"
                        )));
                    }
                }
            }
        }
    }
    Ok(())
}

fn child_full_name(parent: &str, child: &str) -> String {
    format!("{parent}-{child}")
}

/// Does this domain currently have an active owner? In Trelane's registry a
/// domain IS its owning agent; "ownerless" means the agent is disabled.
fn domain_owned(conn: &Connection, agent: &str) -> Result<bool> {
    Ok(store::agent_exists(conn, agent)?
        && store::session_agent_enabled(conn, agent)?.unwrap_or(true))
}

/// The outcome of one leaf's split.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SplitOutcome {
    /// Ownerless domain: children registered directly at the next tier.
    Applied(Vec<String>),
    /// Owned domain: a review proposal was filed instead (R20); the owner's
    /// scope is untouched.
    Proposed(String),
}

/// Split a leaf domain. Owned -> proposal (R20). Ownerless -> applied
/// directly, producing children at the next tier with sibling adjacency.
pub fn split_domain(
    ctx: &Context,
    parent_name: &str,
    children: &[SplitChild],
    rationale: &str,
    pass_no: i64,
) -> Result<SplitOutcome> {
    let parent = store::get_domain(&ctx.conn, parent_name)?
        .ok_or_else(|| TrelaneError::msg(format!("unknown domain '{parent_name}'")))?;
    validate_children(&ctx.conn, &parent, children)?;

    if domain_owned(&ctx.conn, parent_name)? {
        // R20: a split against an owned domain is a proposal, not a fact.
        let id = crate::crypto::new_id("split");
        let now = crate::crypto::now_iso();
        let proposal_json = serde_json::to_string(&serde_json::json!({
            "children": children,
            "rationale": rationale,
            "pass": pass_no,
            "next_tier": next_tier(&parent.granularity_tier),
        }))?;
        store::insert_split_proposal(
            &ctx.conn,
            &SplitProposal {
                id: id.clone(),
                domain: parent_name.to_string(),
                owner_at_split_time: Some(parent_name.to_string()),
                proposal_json,
                status: "pending".to_string(),
                created_at: now,
                resolved_at: None,
            },
        )?;
        return Ok(SplitOutcome::Proposed(id));
    }

    // Ownerless: apply directly.
    let applied = apply_split_children(ctx, &parent, children, pass_no, true)?;
    Ok(SplitOutcome::Applied(applied))
}

/// Register the children of a split at the next tier, record sibling
/// adjacency, and (when `retire_parent`) retire the parent's coverage.
/// Returns the registered child domain names.
fn apply_split_children(
    ctx: &Context,
    parent: &Domain,
    children: &[SplitChild],
    pass_no: i64,
    retire_parent: bool,
) -> Result<Vec<String>> {
    let now = crate::crypto::now_iso();
    let tier = next_tier(&parent.granularity_tier)
        .ok_or_else(|| TrelaneError::msg("domain is already at the finest named tier"))?;
    let mut child_names: Vec<String> = children
        .iter()
        .map(|c| child_full_name(&parent.agent, &c.name))
        .collect();
    child_names.sort();

    let specs: std::collections::HashMap<&str, &SplitChild> =
        children.iter().map(|c| (c.name.as_str(), c)).collect();
    for full in &child_names {
        let bare = full
            .strip_prefix(&format!("{}-", parent.agent))
            .unwrap_or(full);
        let spec = specs[bare];
        crate::commands::cmd_add_agent(
            ctx,
            full,
            &spec.writable,
            &parent.forbidden_write,
            Some(&spec.description),
            None,
        )?;
        store::set_domain_lineage(&ctx.conn, full, tier, Some(&parent.agent), pass_no, &now)?;
    }

    // Sibling adjacency is free by construction: children of the same split
    // are each other's best next move (5B). Rank is alphabetical for
    // determinism.
    for (i, from) in child_names.iter().enumerate() {
        let mut entries: Vec<(String, i64, String, String)> = Vec::new();
        let mut rank = 1i64;
        for (j, to) in child_names.iter().enumerate() {
            if i == j {
                continue;
            }
            entries.push((
                to.clone(),
                rank,
                format!("split from {} in pass {pass_no}", parent.agent),
                "sibling".to_string(),
            ));
            rank += 1;
        }
        store::replace_adjacency(&ctx.conn, from, &entries, &now)?;
    }

    if retire_parent {
        // R17: the parent remains as lineage but no longer covers anything;
        // its coverage now lives in the children. Never deleted.
        ctx.conn.execute(
            "UPDATE agents SET writable_json = '[]' WHERE id = ?1",
            rusqlite::params![parent.agent],
        )?;
    }
    Ok(child_names)
}

/// Accept a pending split proposal (R20 review): children are registered at
/// the next tier. The owner's own scope stays untouched -- it separately
/// finishes or redomains.
pub fn accept_split(ctx: &Context, id: &str) -> Result<Vec<String>> {
    let proposal = store::get_split_proposal(&ctx.conn, id)?
        .ok_or_else(|| TrelaneError::msg(format!("unknown split proposal '{id}'")))?;
    if proposal.status != "pending" {
        return Err(TrelaneError::msg(format!(
            "split proposal '{id}' is already {}",
            proposal.status
        )));
    }
    let parsed: serde_json::Value = serde_json::from_str(&proposal.proposal_json)?;
    let children: Vec<SplitChild> =
        serde_json::from_value(parsed["children"].clone()).unwrap_or_default();
    let pass_no = parsed["pass"].as_i64().unwrap_or(0);
    let parent = store::get_domain(&ctx.conn, &proposal.domain)?
        .ok_or_else(|| TrelaneError::msg("parent domain not found"))?;
    validate_children(&ctx.conn, &parent, &children)?;
    // The owner accepted: children are created, but the parent's writable
    // scope is NOT retired here (R20).
    let applied = apply_split_children(ctx, &parent, &children, pass_no, false)?;
    store::resolve_split_proposal(&ctx.conn, id, "accepted", &crate::crypto::now_iso())?;
    Ok(applied)
}

pub fn reject_split(ctx: &Context, id: &str) -> Result<()> {
    store::resolve_split_proposal(&ctx.conn, id, "rejected", &crate::crypto::now_iso())
}

// ------------------------------------------------------- adjacency (R21/R22)

/// Recompute sibling adjacency for a set of same-parent children (called on
/// every split; recomputed, never accumulated).
pub fn refresh_sibling_adjacency(ctx: &Context, parent: &Domain) -> Result<()> {
    let now = crate::crypto::now_iso();
    let mut children: Vec<String> = Vec::new();
    for agent in store::list_agents(&ctx.conn)? {
        if let Some(dom) = store::get_domain(&ctx.conn, &agent)?
            && dom.parent_domain.as_deref() == Some(parent.agent.as_str())
        {
            children.push(agent);
        }
    }
    children.sort();
    for (i, from) in children.iter().enumerate() {
        let mut entries = Vec::new();
        let mut rank = 1i64;
        for (j, to) in children.iter().enumerate() {
            if i != j {
                entries.push((
                    to.clone(),
                    rank,
                    format!("split from {} (sibling)", parent.agent),
                    "sibling".to_string(),
                ));
                rank += 1;
            }
        }
        store::replace_adjacency(&ctx.conn, from, &entries, &now)?;
    }
    Ok(())
}

/// The LLM layer of adjacency: rank cross-branch moves between leaves that
/// are NOT siblings. Cheapest layer (siblings) is always written by the
/// split itself; this runs on the same opt-in `--refine` invocation.
pub fn compute_llm_adjacency(
    ctx: &Context,
    model: &str,
    leaves: &[LeafDomain],
) -> Result<usize> {
    if leaves.len() < 2 {
        return Ok(0);
    }
    let leaf_list = leaves
        .iter()
        .map(|l| format!("- {} (tier {}, {} glob(s))", l.name, l.tier, l.writable.len()))
        .collect::<Vec<_>>()
        .join("\n");
    let prompt = format!(
        "You are Biplane, the planning layer of a Trelane swarm. These are the current leaf \
         domains:\n{leaf_list}\n\nFor EACH domain, rank the best OTHER domains an agent should \
         try when its own domain runs out of ready work (adjacency: where to look first, not \
         permission to write). Prefer domains whose work is likely related by feature or file \
         proximity.\n\nReply with STRICT JSON only:\n\
         {{\"moves\": [{{\"from\": \"domain\", \"to\": \"domain\", \"rank\": 1, \"rationale\": \"...\"}}]}}\n\
         Every from/to must be one of the listed domains, never the same domain twice."
    );
    let output = run_model_once(ctx, model, "adjacency", &prompt)?;
    #[derive(serde::Deserialize, Default)]
    struct Moves {
        #[serde(default)]
        moves: Vec<AdjMove>,
    }
    #[derive(serde::Deserialize)]
    struct AdjMove {
        from: String,
        to: String,
        rank: i64,
        #[serde(default)]
        rationale: String,
    }
    let parsed: Moves = parse_json_object(&output)?;
    let valid: std::collections::HashSet<&str> =
        leaves.iter().map(|l| l.name.as_str()).collect();
    let mut grouped: std::collections::HashMap<String, Vec<(String, i64, String, String)>> =
        std::collections::HashMap::new();
    for m in parsed.moves {
        if valid.contains(m.from.as_str()) && valid.contains(m.to.as_str()) && m.from != m.to {
            grouped
                .entry(m.from)
                .or_default()
                .push((m.to, m.rank, m.rationale, "llm".to_string()));
        }
    }
    let now = crate::crypto::now_iso();
    let mut refreshed = 0;
    for (from, mut entries) in grouped {
        entries.sort_by_key(|(_, rank, _, _)| *rank);
        // Renumber defensively so ranks are dense and deterministic.
        let entries: Vec<(String, i64, String, String)> = entries
            .into_iter()
            .enumerate()
            .map(|(i, (to, _, rat, src))| (to, (i + 1) as i64, rat, src))
            .collect();
        store::replace_adjacency(&ctx.conn, &from, &entries, &now)?;
        refreshed += 1;
    }
    Ok(refreshed)
}

// ------------------------------------------------------- wake-context (5B)

/// R21: when an agent's domain has zero ready/open tasks, its ranked
/// adjacency list is attached to the wake context -- detection is
/// mechanical, acting on it is the agent's call.
pub fn exhaustion_adjacency_summary(conn: &Connection, agent: &str) -> Option<String> {
    let ready = store::list_ready_owned_tasks(conn, agent).ok()?;
    if !ready.is_empty() {
        return None;
    }
    let open: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM tasks WHERE owner_agent = ?1 \
             AND state IN ('active', 'blocked', 'review')",
            rusqlite::params![agent],
            |r| r.get(0),
        )
        .ok()?;
    if open > 0 {
        return None;
    }
    let adjacency = store::get_adjacency(conn, agent).ok()?;
    if adjacency.is_empty() {
        return None;
    }
    let mut lines = vec![
        "Your domain has no ready work. Per §6 of the Trelane Protocol, try these adjacent \
         domains in order instead of going idle:"
            .to_string(),
    ];
    for a in &adjacency {
        lines.push(format!(
            "{}. {} -- {} [{}]",
            a.rank, a.to_domain, a.rationale, a.source
        ));
    }
    lines.push(
        "Unowned target: claim it and announce on the bulletin (free move). Owned target: \
         `trelane di request` or a handoff -- adjacency says where to look, not what you may \
         write (R22)."
            .to_string(),
    );
    Some(lines.join("\n"))
}

/// R29: unreviewed split proposals against the agent's domain, attached to
/// its wake -- informational, not blocking, not a wake reason of its own.
pub fn pending_split_summary(conn: &Connection, agent: &str) -> Option<String> {
    let proposals = store::list_pending_split_proposals_for(conn, agent).ok()?;
    if proposals.is_empty() {
        return None;
    }
    let mut lines = vec![
        "Biplane has proposed splitting your domain. Informational only -- your current scope \
         is untouched until you finish or redomain (R20). Review with:"
            .to_string(),
    ];
    for p in &proposals {
        let children: serde_json::Value =
            serde_json::from_str(&p.proposal_json).unwrap_or_default();
        let rationale = children["rationale"].as_str().unwrap_or("");
        lines.push(format!(
            "- {} (proposed {}): {}  |  `trelane split show {0}` · `trelane split accept {0}` · `trelane split reject {0}`",
            p.id, p.created_at, rationale
        ));
    }
    Some(lines.join("\n"))
}

// ------------------------------------------------------------- the pass

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct RefineReport {
    pub pass_no: i64,
    pub splits_applied: Vec<String>,
    pub proposals_created: Vec<String>,
    pub adjacency_domains_refreshed: usize,
    pub skipped: Vec<(String, String)>,
}

/// Run one refinement pass over the current leaf domains. Explicit and
/// opt-in only -- this is the deliberate, model-calling Biplane invocation
/// R19 allows. Never called from the squire.
pub fn refine(
    ctx: &Context,
    model: &str,
    decider: &dyn SplitDecider,
) -> Result<RefineReport> {
    let pass_no = store::next_refinement_pass(&ctx.conn)?;
    let leaves = leaf_domains(&ctx.conn)?;
    let max_tier = ctx.config.biplane.max_granularity_tier.clone();
    let mut report = RefineReport {
        pass_no,
        ..Default::default()
    };

    for leaf in &leaves {
        // R18: one rung at a time, capped by biplane.max_granularity_tier.
        let Some(next) = next_tier(&leaf.tier) else {
            report
                .skipped
                .push((leaf.name.clone(), "at finest named tier".to_string()));
            continue;
        };
        if tier_rank(next) > tier_rank(&max_tier) {
            report
                .skipped
                .push((leaf.name.clone(), format!("capped at {max_tier}")));
            continue;
        }
        let dom = store::get_domain(&ctx.conn, &leaf.name)?
            .ok_or_else(|| TrelaneError::msg("domain vanished mid-pass"))?;
        let growth = gather_growth(ctx, leaf, &dom)?;
        if growth.file_count == 0 {
            report
                .skipped
                .push((leaf.name.clone(), "no files in domain".to_string()));
            continue;
        }
        let decision = decider.decide(leaf, &growth)?;
        if !decision.split {
            report.skipped.push((
                leaf.name.clone(),
                if decision.rationale.is_empty() {
                    "model chose to stay at current tier".to_string()
                } else {
                    decision.rationale.clone()
                },
            ));
            continue;
        }
        match split_domain(ctx, &leaf.name, &decision.children, &decision.rationale, pass_no)? {
            SplitOutcome::Applied(children) => {
                report
                    .splits_applied
                    .push(format!("{} -> [{}]", leaf.name, children.join(", ")));
            }
            SplitOutcome::Proposed(id) => {
                report
                    .proposals_created
                    .push(format!("{} (proposal {id})", leaf.name));
            }
        }
    }

    // Cross-branch adjacency, same opt-in invocation (5B second layer).
    let leaves_after = leaf_domains(&ctx.conn)?;
    report.adjacency_domains_refreshed = compute_llm_adjacency(ctx, model, &leaves_after)?;
    Ok(report)
}

// --------------------------------------------------------------------- CLI

/// `trelane biplane --refine`: one progressive-refinement pass (Slice 5).
/// This is the deliberate, model-calling invocation R19 allows -- it is
/// never reachable from the squire's tick path.
pub fn cmd_refine(ctx: &Context, model: &str, json: bool) -> Result<()> {
    let decider = LlmSplitDecider { ctx, model };
    let report = refine(ctx, model, &decider)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    println!("refinement pass {} complete", report.pass_no);
    if !report.splits_applied.is_empty() {
        println!("  splits applied (ownerless domains):");
        for s in &report.splits_applied {
            println!("    + {s}");
        }
    }
    if !report.proposals_created.is_empty() {
        println!("  split proposals filed for owner review (R20):");
        for p in &report.proposals_created {
            println!("    ? {p}");
        }
        println!("  owners review with: trelane split show|accept|reject <id>");
    }
    if report.adjacency_domains_refreshed > 0 {
        println!(
            "  adjacency refreshed for {} domain(s)",
            report.adjacency_domains_refreshed
        );
    }
    if !report.skipped.is_empty() {
        println!("  skipped:");
        for (name, why) in &report.skipped {
            println!("    - {name}: {why}");
        }
    }
    Ok(())
}

/// `trelane split ...`: review split proposals (R20/R29).
pub fn cmd_split(ctx: &Context, action: &crate::cli::SplitAction) -> Result<()> {
    match action {
        crate::cli::SplitAction::List { status, json } => {
            let proposals = store::list_split_proposals(&ctx.conn, status.as_deref())?;
            if *json {
                let out: Vec<serde_json::Value> = proposals
                    .iter()
                    .map(|p| serde_json::json!({
                        "id": p.id,
                        "domain": p.domain,
                        "status": p.status,
                        "created_at": p.created_at,
                        "resolved_at": p.resolved_at,
                    }))
                    .collect();
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else {
                if proposals.is_empty() {
                    println!("(no split proposals)");
                }
                for p in &proposals {
                    println!("{}  {:<9} {}  ({})", p.id, p.status, p.domain, p.created_at);
                }
            }
            Ok(())
        }
        crate::cli::SplitAction::Show { id, json } => {
            let p = store::get_split_proposal(&ctx.conn, id)?
                .ok_or_else(|| TrelaneError::msg(format!("unknown split proposal '{id}'")))?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&serde_json::json!({
                    "id": p.id,
                    "domain": p.domain,
                    "status": p.status,
                    "created_at": p.created_at,
                    "resolved_at": p.resolved_at,
                    "proposal": serde_json::from_str::<serde_json::Value>(&p.proposal_json)?,
                }))?);
            } else {
                println!("id      : {}", p.id);
                println!("domain  : {}", p.domain);
                println!("status  : {}", p.status);
                println!("created : {}", p.created_at);
                let parsed: serde_json::Value =
                    serde_json::from_str(&p.proposal_json).unwrap_or_default();
                println!("rationale: {}", parsed["rationale"].as_str().unwrap_or(""));
                if let Some(children) = parsed["children"].as_array() {
                    println!("children:");
                    for c in children {
                        let writable = c["writable"]
                            .as_array()
                            .map(|w| {
                                w.iter()
                                    .filter_map(|g| g.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            })
                            .unwrap_or_default();
                        println!(
                            "  - {}: {} ({})",
                            c["name"].as_str().unwrap_or("?"),
                            c["description"].as_str().unwrap_or(""),
                            writable
                        );
                    }
                }
                if p.status == "pending" {
                    println!();
                    println!("review: trelane split accept {id}  |  trelane split reject {id}");
                }
            }
            Ok(())
        }
        crate::cli::SplitAction::Accept { id } => {
            let applied = accept_split(ctx, id)?;
            println!("accepted {id}; registered: {}", applied.join(", "));
            println!("your own scope is unchanged until you finish or redomain (R20)");
            Ok(())
        }
        crate::cli::SplitAction::Reject { id } => {
            reject_split(ctx, id)?;
            println!("rejected {id}");
            Ok(())
        }
    }
}
