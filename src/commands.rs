use crate::Context;
use crate::crypto;
use crate::domain::{self, CompiledDomain};
use crate::error::{Result, TrelaneError};
use crate::models::*;
use crate::prompt;
use crate::store;
use rusqlite::Connection;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

// ----------------------------------------------------------------- helpers

pub fn is_running(conn: &Connection, agent: &str) -> Result<bool> {
    match store::get_running_lock(conn, agent)? {
        None => Ok(false),
        Some(lock) => {
            let age_s = chrono::DateTime::parse_from_rfc3339(&lock.started_at)
                .ok()
                .map(|dt| {
                    chrono::Utc::now()
                        .signed_duration_since(dt.with_timezone(&chrono::Utc))
                        .num_seconds()
                })
                .unwrap_or(0);

            // Hard timeout: any running lock older than 5 minutes is stale,
            // regardless of PID. This prevents stale locks when opencode
            // sessions crash or are closed without calling 'trelane done'.
            if age_s > 300 {
                eprintln!(
                    "warning: clearing stale running lock for {agent} (age={age_s}s, pid={})",
                    lock.pid
                );
                store::delete_running_lock(conn, agent)?;
                return Ok(false);
            }

            if lock.pid <= 0 {
                return Ok(true);
            }

            let alive = unsafe { libc::kill(lock.pid, 0) == 0 };
            if !alive {
                store::delete_running_lock(conn, agent)?;
            }
            Ok(alive)
        }
    }
}

pub fn clear_all_stale_locks(conn: &Connection) -> Result<usize> {
    let agents = store::list_agents(conn)?;
    let mut cleared = 0;
    for agent in &agents {
        if let Some(lock) = store::get_running_lock(conn, agent)? {
            let age_s = chrono::DateTime::parse_from_rfc3339(&lock.started_at)
                .ok()
                .map(|dt| {
                    chrono::Utc::now()
                        .signed_duration_since(dt.with_timezone(&chrono::Utc))
                        .num_seconds()
                })
                .unwrap_or(0);

            let stale = age_s > 300 || (lock.pid > 0 && unsafe { libc::kill(lock.pid, 0) != 0 });

            if stale {
                eprintln!(
                    "clearing stale lock for {agent} (age={age_s}s, pid={})",
                    lock.pid
                );
                store::delete_running_lock(conn, agent)?;
                cleared += 1;
            }
        }
    }
    Ok(cleared)
}

fn is_valid_agent_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 32 {
        return false;
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return false;
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

pub fn owners_of(conn: &Connection, rel: &str, exclude: Option<&str>) -> Result<Vec<String>> {
    let agents = store::list_agents(conn)?;
    let mut owners = Vec::new();
    for agent in &agents {
        if Some(agent.as_str()) == exclude {
            continue;
        }
        if let Some(dom) = store::get_domain(conn, agent)?
            && CompiledDomain::from_domain(&dom)?.is_writable(rel)
        {
            owners.push(agent.clone());
        }
    }
    Ok(owners)
}

fn normalize_scope_entry(root: &Path, value: &str) -> Result<String> {
    if value.trim().is_empty() {
        return Err(TrelaneError::msg("path scope entries cannot be empty"));
    }
    if Path::new(value).is_absolute() {
        return domain::norm_rel(root, value);
    }
    let normalized = value.replace(std::path::MAIN_SEPARATOR, "/");
    let normalized = normalized.strip_prefix("./").unwrap_or(&normalized);
    if normalized
        .split('/')
        .any(|part| part == ".." || part.is_empty())
    {
        return Err(TrelaneError::msg(format!(
            "invalid project-relative path scope '{value}'"
        )));
    }
    Ok(normalized.to_string())
}

fn normalize_scope(root: &Path, values: &[String]) -> Result<Vec<String>> {
    values
        .iter()
        .map(|value| normalize_scope_entry(root, value))
        .collect()
}

fn scope_is_subset(candidates: &[String], parents: &[String]) -> Result<bool> {
    for candidate in candidates {
        let mut proved = false;
        for parent in parents {
            if domain::scope_entry_is_subset(candidate, parent)? {
                proved = true;
                break;
            }
        }
        if !proved {
            return Ok(false);
        }
    }
    Ok(true)
}

fn validate_delegable_scope(
    ctx: &Context,
    task: &Task,
    owner: &str,
    values: &[String],
) -> Result<Vec<String>> {
    if values.is_empty() {
        return Err(TrelaneError::msg(
            "help requires at least one path scope (the task has no usable default)",
        ));
    }
    let scope = normalize_scope(&ctx.root, values)?;
    if !scope_is_subset(&scope, &task.path_scope)? {
        return Err(TrelaneError::msg(
            "proposed path scope is not a provable subset of the task scope",
        ));
    }
    let owner_domain = store::get_domain(&ctx.conn, owner)?
        .ok_or_else(|| TrelaneError::msg(format!("unknown owner agent '{owner}'")))?;
    for entry in &scope {
        if !domain::domain_allows_scope(&owner_domain, entry)? {
            return Err(TrelaneError::msg(format!(
                "path scope '{entry}' is outside the owner's writable scope or intersects a forbidden path"
            )));
        }
    }
    Ok(scope)
}

fn delegation_expiry(delegation: &Delegation) -> Result<chrono::DateTime<chrono::Utc>> {
    let value = delegation
        .expires_at
        .as_deref()
        .ok_or_else(|| TrelaneError::msg("delegation has no expiry"))?;
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .map_err(|_| TrelaneError::msg("delegation has an invalid expiry"))
}

fn grant_message_verifies(
    ctx: &Context,
    delegation: &Delegation,
    explicit_grant: Option<&str>,
) -> Result<bool> {
    if delegation.grant_message.is_empty()
        || explicit_grant.is_some_and(|id| id != delegation.grant_message)
    {
        return Ok(false);
    }
    let Some(msg) = store::get_message(&ctx.conn, &delegation.grant_message)? else {
        return Ok(false);
    };
    let body: serde_json::Value = match serde_json::from_str(&msg.body) {
        Ok(body) => body,
        Err(_) => return Ok(false),
    };
    Ok(msg.from == delegation.owner_agent
        && msg.to == delegation.helper_agent
        && msg.msg_type == "help-accept"
        && msg.task.as_deref() == Some(delegation.task_id.as_str())
        && msg.re.as_deref() == Some(delegation.offer_message.as_str())
        && msg.paths == delegation.scope
        && body.get("delegation_id").and_then(|v| v.as_str()) == Some(delegation.id.as_str())
        && body.get("allowed_ops")
            == Some(&serde_json::to_value(&delegation.allowed_ops).unwrap_or_default())
        && body.get("expires_at").and_then(|v| v.as_str()) == delegation.expires_at.as_deref()
        && crypto::verify(&ctx.secret()?, &msg))
}

fn authorize_delegated_claim(
    ctx: &Context,
    helper: &str,
    delegation_id: &str,
    explicit_grant: Option<&str>,
    task_id: Option<&str>,
    rel: &str,
) -> Result<(Delegation, chrono::DateTime<chrono::Utc>)> {
    let delegation = store::get_delegation(&ctx.conn, delegation_id)?
        .ok_or_else(|| TrelaneError::msg(format!("no delegation '{delegation_id}'")))?;
    if delegation.status != DelegationStatus::Active {
        return Err(TrelaneError::msg(format!(
            "delegation '{delegation_id}' is {}, not active",
            delegation.status.as_str()
        )));
    }
    if delegation.helper_agent != helper {
        return Err(TrelaneError::msg(format!(
            "delegation '{delegation_id}' is for helper '{}', not '{helper}'",
            delegation.helper_agent
        )));
    }
    if task_id.is_some_and(|task| task != delegation.task_id) {
        return Err(TrelaneError::msg(
            "claim task does not match the delegation task",
        ));
    }
    let task = store::get_task(&ctx.conn, &delegation.task_id)?
        .ok_or_else(|| TrelaneError::msg("delegation task no longer exists"))?;
    if task.state.is_terminal() {
        return Err(TrelaneError::msg("delegation task is terminal"));
    }
    if !domain::scope_covers_path(&task.path_scope, rel)? {
        return Err(TrelaneError::msg(format!(
            "task scope does not cover '{rel}'"
        )));
    }
    let expiry = delegation_expiry(&delegation)?;
    if expiry <= chrono::Utc::now() {
        return Err(TrelaneError::msg(format!(
            "delegation '{delegation_id}' is expired"
        )));
    }
    if domain::is_hard_forbidden(rel) {
        return Err(TrelaneError::msg(format!(
            "hard-forbidden path '{rel}' cannot be delegated"
        )));
    }
    if !delegation.allowed_ops.iter().any(|op| op == "write") {
        return Err(TrelaneError::msg(
            "delegation does not allow the write operation",
        ));
    }
    if !domain::scope_covers_path(&delegation.scope, rel)? {
        return Err(TrelaneError::msg(format!(
            "delegation scope does not cover '{rel}'"
        )));
    }
    let owner_domain = store::get_domain(&ctx.conn, &delegation.owner_agent)?
        .ok_or_else(|| TrelaneError::msg("delegation owner no longer exists"))?;
    if !CompiledDomain::from_domain(&owner_domain)?.is_writable(rel) {
        return Err(TrelaneError::msg(format!(
            "delegation owner '{}' no longer owns '{rel}'",
            delegation.owner_agent
        )));
    }
    if !grant_message_verifies(ctx, &delegation, explicit_grant)? {
        return Err(TrelaneError::msg(
            "delegation's signed help-accept grant is missing, invalid, or mismatched",
        ));
    }
    Ok((delegation, expiry))
}

fn git_dirty(root: &Path) -> Option<HashMap<String, String>> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["status", "--porcelain=v1"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut dirty = HashMap::new();
    for line in stdout.lines() {
        if line.len() < 3 {
            continue;
        }
        let rel = line[3..]
            .split(" -> ")
            .last()
            .unwrap_or("")
            .trim()
            .trim_matches('"')
            .replace(std::path::MAIN_SEPARATOR, "/");
        if rel.is_empty() || rel.starts_with(".trelane/") || rel == ".trelane" {
            continue;
        }
        let rel_owned = rel.clone();
        dirty.insert(rel_owned, hash_file(root, &rel));
    }
    Some(dirty)
}

fn hash_file(root: &Path, rel: &str) -> String {
    let path = root.join(rel);
    if !path.is_file() {
        return "absent".to_string();
    }
    use sha2::{Digest, Sha256};
    let data = std::fs::read(&path).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(&data);
    crypto::hex_encode(&hasher.finalize())
}

// ------------------------------------------------------------------- init

pub fn cmd_init(project: Option<PathBuf>) -> Result<()> {
    let root = match project {
        Some(p) => p.canonicalize()?,
        None => std::env::current_dir()?.canonicalize()?,
    };
    let trelane_dir = root.join(TRELANE_DIR);

    if trelane_dir.join("trelane.db").exists() {
        return Err(TrelaneError::msg(format!(
            "{} already initialized",
            trelane_dir.display()
        )));
    }

    std::fs::create_dir_all(trelane_dir.join("agents"))?;
    std::fs::create_dir_all(trelane_dir.join("prompts"))?;

    // Global config lives at ~/.config/trelane/config.json, not per-project.
    let config_path = crate::ensure_config()?;

    let secret = crypto::generate_secret();
    std::fs::write(trelane_dir.join("secret"), &secret)?;

    std::fs::write(
        trelane_dir.join("prompts").join("bootstrap.md"),
        prompt::bootstrap_template(),
    )?;

    std::fs::write(
        trelane_dir.join(".gitignore"),
        "secret\nagents/*/.prompt.md\nagents/*/logs/\nsquire.log\nsquire.log\n*.db-wal\n*.db-shm\n",
    )?;

    let db_path = trelane_dir.join("trelane.db");
    let conn = crate::db::open(&db_path)?;
    drop(conn);

    println!("initialized trelane at {}", trelane_dir.display());
    println!("config: {}", config_path.display());
    println!("next: trelane add-agent <name> --writable '<glob>' ...");
    Ok(())
}

// ----------------------------------------------------------------- attach

pub fn parse_agent_list(input: Option<&str>) -> Vec<String> {
    input
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

pub fn selected_agents(
    config: &Config,
    agents: Option<&str>,
    no_agents: Option<&str>,
) -> Vec<String> {
    let mut enabled = config.agents.default.clone();
    enabled.extend(parse_agent_list(agents));
    let mut disabled = config.agents.disabled.clone();
    disabled.extend(parse_agent_list(no_agents));

    enabled.sort();
    enabled.dedup();
    disabled.sort();
    disabled.dedup();

    enabled
        .into_iter()
        .filter(|name| !disabled.contains(name))
        .collect()
}

pub fn cmd_attach_project(
    project: Option<PathBuf>,
    agents: Option<&str>,
    no_agents: Option<&str>,
    inject: bool,
) -> Result<()> {
    let root = match project {
        Some(p) => p.canonicalize()?,
        None => std::env::current_dir()?.canonicalize()?,
    };

    if !root.join(TRELANE_DIR).join("trelane.db").exists() {
        cmd_init(Some(root.clone()))?;
    }

    let ctx = Context::open(Some(&root))?;
    let enabled = selected_agents(&ctx.config, agents, no_agents);
    let disabled = {
        let mut d = ctx.config.agents.disabled.clone();
        d.extend(parse_agent_list(no_agents));
        d.sort();
        d.dedup();
        d
    };
    let now = crypto::now_iso();
    for name in &enabled {
        store::upsert_session_agent(&ctx.conn, name, true, "attach", &now)?;
    }
    for name in &disabled {
        store::upsert_session_agent(&ctx.conn, name, false, "attach", &now)?;
        // F1: Proactively resolve dangling parks for this disabled agent
        // so waiters are woken immediately, not on the next squire tick.
        if let Err(e) = resolve_dangling_parks_for(&ctx, name) {
            eprintln!("warning: failed to resolve dangling parks for {name}: {e:?}");
        }
    }

    if inject {
        inject_agents_md(&ctx.root, &enabled, &disabled)?;
    }

    println!("attached trelane session at {}", ctx.root.display());
    println!("enabled agents: {}", list_or_none(&enabled));
    println!("disabled agents: {}", list_or_none(&disabled));
    if inject {
        println!("updated {}", ctx.root.join("AGENTS.md").display());
    }
    Ok(())
}

fn list_or_none(items: &[String]) -> String {
    if items.is_empty() {
        "(none)".to_string()
    } else {
        items.join(", ")
    }
}

fn is_agent_enabled(ctx: &Context, agent: &str) -> Result<bool> {
    Ok(store::session_agent_enabled(&ctx.conn, agent)?.unwrap_or(true))
}

/// Agents whose launcher mapping is enabled for this session, i.e. agents
/// that `cmd_wake` would not refuse. Launch uses this so frames are only
/// provisioned for agents that can actually run.
pub fn launch_enabled_agents(ctx: &Context) -> Result<Vec<String>> {
    let mut enabled = Vec::new();
    for agent in store::list_agents(&ctx.conn)? {
        if domain_launch_enabled(ctx, &agent)? {
            enabled.push(agent);
        }
    }
    Ok(enabled)
}

fn domain_launch_enabled(ctx: &Context, domain_agent: &str) -> Result<bool> {
    let domain = store::get_domain(&ctx.conn, domain_agent)?
        .ok_or_else(|| TrelaneError::msg(format!("unknown agent '{domain_agent}'")))?;
    match domain.launcher_agent.as_deref() {
        Some(session_agent) => is_agent_enabled(ctx, session_agent),
        None => Ok(true),
    }
}

#[cfg(test)]
fn relaunch_command_for_agent(ctx: &Context, agent: &str) -> String {
    format!(
        "trelane --root {} inbox {} --json",
        ctx.root.display(),
        agent
    )
}

#[allow(dead_code)]
fn applescript_escape(input: &str) -> String {
    input.replace('\\', "\\\\").replace('"', "\\\"")
}

fn shell_double_quote(input: &str) -> String {
    format!("\"{}\"", input.replace('\\', "\\\\").replace('"', "\\\""))
}

fn shell_single_quote(input: &str) -> String {
    format!("'{}'", input.replace('\'', "'\"'\"'"))
}

fn command_for_launch_target(target: &LaunchTarget) -> String {
    match target.tmux_target.as_deref() {
        Some(tmux_target) if target.adapter != "tmux" => format!(
            "tmux send-keys -t {} {} Enter",
            shell_single_quote(tmux_target),
            shell_single_quote(&target.command)
        ),
        _ => target.command.clone(),
    }
}

fn creatable_tmux_session_name(target: &str) -> Option<&str> {
    if target.is_empty() || target.starts_with('%') || target.starts_with('@') {
        return None;
    }
    target.split(':').next().filter(|name| !name.is_empty())
}

pub fn ensure_tmux_target(target: &str) -> Result<()> {
    let has = Command::new("tmux")
        .args(["has-session", "-t", target])
        .status()?;
    if has.success() {
        return Ok(());
    }

    let session_name = creatable_tmux_session_name(target).ok_or_else(|| {
        TrelaneError::msg(format!(
            "tmux target '{target}' does not exist and cannot be auto-created"
        ))
    })?;
    let create = Command::new("tmux")
        .args(["new-session", "-d", "-s", session_name])
        .status()?;
    if !create.success() {
        return Err(TrelaneError::msg(format!(
            "failed to auto-create tmux session '{session_name}' for target '{target}'"
        )));
    }
    Ok(())
}

fn launcher_command_for_agent(
    ctx: &Context,
    agent: &str,
    prompt_file: &Path,
    launcher_override: Option<&str>,
) -> Result<String> {
    if let Some(override_cmd) = launcher_override {
        return Ok(override_cmd
            .replace("{prompt_file}", &prompt_file.display().to_string())
            .replace("{agent}", agent)
            .replace("{root}", &ctx.root.display().to_string()));
    }

    let domain = store::get_domain(&ctx.conn, agent)?
        .ok_or_else(|| TrelaneError::msg(format!("unknown agent '{agent}'")))?;

    let template = match domain.launcher_agent.as_deref() {
        // A configured launcher PROFILE name (claude-code/opencode/copilot/...).
        Some(name) if ctx.config.launcher.profiles.contains_key(name) => {
            ctx.config.launcher.profiles.get(name).unwrap().clone()
        }
        // Any other non-empty value is treated as an exact opencode model id
        // (this is what the Biplane UI's model selector stores -- raw lines
        // from `opencode models`, e.g. "openrouter/z-ai/glm-5.2"). Without
        // this branch such a value matched no profile and silently fell back
        // to the default launcher template, so a model chosen in the UI never
        // actually took effect. Building an explicit opencode+model command
        // here mirrors the same pattern Biplane's own planning call uses.
        Some(model_id) if !model_id.is_empty() => {
            format!("opencode run --model {model_id} --dir {{root}} \"$(cat {{prompt_file}})\"")
        }
        // No launcher was ever explicitly chosen for this agent. Do NOT
        // silently fall back to the global default template: that default is
        // a real CLI invocation (out of the box, Anthropic's `claude`) that
        // can bill the user's account. Launching that without an explicit,
        // per-agent choice risks unintended real-money spend, so refuse
        // instead -- this must never happen implicitly.
        _ => return Err(TrelaneError::launcher_not_configured(agent)),
    };

    Ok(template
        .replace("{prompt_file}", &prompt_file.display().to_string())
        .replace("{agent}", agent)
        .replace("{root}", &ctx.root.display().to_string()))
}

fn launch_via_adapter(adapter: &str, target: &str, command: &str) -> Result<()> {
    if adapter == "tmux" {
        ensure_tmux_target(target)?;
    }
    let status = match adapter {
        "tmux" => Command::new("tmux")
            .args(["send-keys", "-t", target, command, "Enter"])
            .status()?,
        // GUI terminal adapters removed: osascript calls trigger repeated
        // macOS Automation permission prompts and are not needed now that
        // Trelane is tmux-first.  Users who need a non-tmux adapter can
        // write a custom --command on set-launch-target.
        other => {
            return Err(TrelaneError::msg(format!(
                "unsupported adapter '{other}'. Trelane is tmux-first; use 'tmux' or set a custom --command."
            )));
        }
    };

    if !status.success() {
        return Err(TrelaneError::msg(format!(
            "adapter '{adapter}' failed for target '{target}'"
        )));
    }
    Ok(())
}

fn inject_agents_md(root: &Path, enabled: &[String], disabled: &[String]) -> Result<()> {
    let path = root.join("AGENTS.md");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let start = "<!-- BEGIN TRELANE -->";
    let end = "<!-- END TRELANE -->";
    let block = format!(
        "{start}\n\
## Trelane Coordination\n\n\
This project is attached to a Trelane session. Before each substantial action, check Trelane state and use the protocol instead of waiting on other agents.\n\n\
- Project root: `{}`\n\
- Enabled agents/models: {}\n\
- Disabled agents/models: {}\n\n\
Rules for agents:\n\n\
1. Start by running `trelane status` and `trelane inbox <your-agent-id> --json`.\n\
2. Never wait while running. If blocked, send a message with `trelane send`, then park the task with `trelane park`.\n\
3. Stay in your domain. Use `trelane claim <your-agent-id> <path>` before editing contested or cross-domain files.\n\
4. Run `trelane audit <your-agent-id>` before exiting, then `trelane done <your-agent-id>`.\n\
5. If woken for a deadlock, proceed with a documented assumption, notify the counterpart, and unpark the task.\n\n\
Useful commands:\n\n\
```bash\n\
trelane status\n\
trelane inbox <agent> --json\n\
trelane send --from <agent> --to <agent> --type question --subject \"...\" --body \"...\"\n\
trelane park <agent> --wait-reply <msg-id> --waiting-on <agent> --resume-hint \"...\"\n\
trelane claim <agent> <path>\n\
trelane audit <agent>\n\
trelane done <agent>\n\
```\n\
{end}\n",
        root.display(),
        list_or_none(enabled),
        list_or_none(disabled)
    );

    let updated = if let (Some(s), Some(e)) = (existing.find(start), existing.find(end)) {
        let e = e + end.len();
        format!("{}{}{}", &existing[..s], block, &existing[e..])
    } else if existing.trim().is_empty() {
        block
    } else {
        format!("{}\n\n{}", existing.trim_end(), block)
    };
    std::fs::write(path, updated)?;
    Ok(())
}

// -------------------------------------------------------------- add-agent

pub fn cmd_add_agent(
    ctx: &Context,
    name: &str,
    writable: &[String],
    forbidden_write: &[String],
    desc: Option<&str>,
    launcher_agent: Option<&str>,
) -> Result<()> {
    if !is_valid_agent_name(name) {
        return Err(TrelaneError::msg(
            "agent names: lowercase alnum, '-', '_' (max 32 chars)",
        ));
    }
    if store::agent_exists(&ctx.conn, name)? {
        return Err(TrelaneError::msg(format!("agent '{name}' already exists")));
    }
    if let Some(session_agent) = launcher_agent
        && !is_agent_enabled(ctx, session_agent)?
    {
        return Err(TrelaneError::msg(format!(
            "session agent/model '{session_agent}' is disabled for this session"
        )));
    }

    let mut forbidden = vec![format!("{TRELANE_DIR}/**"), ".git/**".to_string()];
    forbidden.extend(forbidden_write.iter().cloned());
    let agent_dir = ctx.trelane_dir().join("agents").join(name);
    std::fs::create_dir_all(agent_dir.join("logs"))?;

    store::insert_agent(
        &ctx.conn,
        name,
        desc.unwrap_or(""),
        writable,
        launcher_agent,
        &forbidden,
        &crypto::now_iso(),
    )?;

    if let Some(session_agent) = launcher_agent {
        let now = crypto::now_iso();
        store::upsert_session_agent(&ctx.conn, session_agent, true, "add-agent", &now)?;
    }

    println!("added agent '{name}' writable={writable:?}");
    Ok(())
}

pub fn cmd_redomain(
    ctx: &Context,
    agent: &str,
    writable: &[String],
    forbidden_write: &[String],
    desc: Option<&str>,
) -> Result<()> {
    if !store::agent_exists(&ctx.conn, agent)? {
        return Err(TrelaneError::msg(format!("unknown agent '{agent}'")));
    }

    let existing = store::get_domain(&ctx.conn, agent)?
        .ok_or_else(|| TrelaneError::msg(format!("unknown agent '{agent}'")))?;

    let mut forbidden = vec![format!("{TRELANE_DIR}/**"), ".git/**".to_string()];
    forbidden.extend(forbidden_write.iter().cloned());
    let now = crypto::now_iso();
    store::upsert_agent(
        &ctx.conn,
        agent,
        desc.unwrap_or(&existing.description),
        writable,
        existing.launcher_agent.as_deref(),
        &forbidden,
        &now,
    )?;

    for other in store::list_agents(&ctx.conn)? {
        if other == agent {
            continue;
        }
        let mut msg = Message::new(
            crypto::new_id("msg"),
            agent.to_string(),
            other,
            "info".to_string(),
            "normal".to_string(),
            format!("domain updated: {agent}"),
            format!(
                "Agent '{agent}' updated its writable globs to: {}",
                writable.join(", ")
            ),
            None,
            None,
            vec![],
            crypto::now_iso(),
        );
        let secret = ctx.secret()?;
        crypto::sign(&secret, &mut msg);
        store::insert_message(&ctx.conn, &msg)?;
    }

    println!("updated domain for '{agent}' writable={writable:?}");
    Ok(())
}

// ------------------------------------------------------------------- send

#[allow(clippy::too_many_arguments)]
pub fn cmd_send(
    ctx: &Context,
    from: &str,
    to: &str,
    msg_type: &str,
    urgency: &str,
    subject: &str,
    body: &str,
    re: &Option<String>,
    task: &Option<String>,
    paths: &[String],
) -> Result<()> {
    if from != "user" && !store::agent_exists(&ctx.conn, from)? {
        return Err(TrelaneError::msg(format!("unknown agent '{from}'")));
    }
    if to == "user" {
        return Err(TrelaneError::msg(
            "'user' has no inbox; write your findings to your run output instead",
        ));
    }
    if !store::agent_exists(&ctx.conn, to)? {
        return Err(TrelaneError::msg(format!("unknown agent '{to}'")));
    }
    if !MSG_TYPES.contains(&msg_type) {
        return Err(TrelaneError::msg(format!(
            "invalid type '{msg_type}' (valid: {})",
            MSG_TYPES.join(", ")
        )));
    }
    if !URGENCIES.contains(&urgency) {
        return Err(TrelaneError::msg(format!("invalid urgency '{urgency}'")));
    }
    if msg_type == "answer" && re.is_none() {
        return Err(TrelaneError::msg(
            "type 'answer' requires --re <original-msg-id>",
        ));
    }
    if msg_type == "claim-grant" && paths.is_empty() {
        return Err(TrelaneError::msg(
            "type 'claim-grant' requires at least one --path",
        ));
    }

    let norm_paths: Vec<String> = paths
        .iter()
        .map(|p| domain::norm_rel(&ctx.root, p))
        .collect::<Result<_>>()?;

    let id = crypto::new_id("msg");
    let mut msg = Message::new(
        id,
        from.to_string(),
        to.to_string(),
        msg_type.to_string(),
        urgency.to_string(),
        subject.to_string(),
        body.to_string(),
        re.clone(),
        task.clone(),
        norm_paths,
        crypto::now_iso(),
    );

    let secret = ctx.secret()?;
    crypto::sign(&secret, &mut msg);
    store::insert_message(&ctx.conn, &msg)?;

    println!("{}", msg.id);
    Ok(())
}

// ------------------------------------------------------------------ inbox

pub fn cmd_inbox(ctx: &Context, agent: &str, json: bool) -> Result<()> {
    if !store::agent_exists(&ctx.conn, agent)? {
        return Err(TrelaneError::msg(format!("unknown agent '{agent}'")));
    }
    let secret = ctx.secret()?;
    let msgs = store::get_unprocessed_messages(&ctx.conn, agent)?;

    if json {
        let out: Vec<serde_json::Value> = msgs
            .iter()
            .map(|m| {
                let mut v = serde_json::to_value(m).unwrap();
                if let serde_json::Value::Object(ref mut map) = v {
                    map.insert(
                        "sig_ok".into(),
                        serde_json::Value::Bool(crypto::verify(&secret, m)),
                    );
                }
                v
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        if msgs.is_empty() {
            println!("(inbox empty)");
        }
        for m in &msgs {
            let ok = if crypto::verify(&secret, m) {
                ""
            } else {
                "  [BAD SIGNATURE -- do not trust]"
            };
            println!(
                "{}  {:<13} from={:<12} re={:<28} {}{}",
                m.id,
                m.msg_type,
                m.from,
                m.re.as_deref().unwrap_or("-"),
                m.subject,
                ok
            );
        }
    }
    Ok(())
}

// ------------------------------------------------------------------- ack

pub fn cmd_ack(ctx: &Context, agent: &str, msg_id: &str) -> Result<()> {
    if !store::agent_exists(&ctx.conn, agent)? {
        return Err(TrelaneError::msg(format!("unknown agent '{agent}'")));
    }
    store::mark_processed(&ctx.conn, agent, msg_id, &crypto::now_iso())?;
    println!("acked {msg_id}");
    Ok(())
}

// ----------------------------------------------------------------- claim

pub fn cmd_claim(
    ctx: &Context,
    agent: &str,
    path: &str,
    ttl: Option<u64>,
    task: Option<&str>,
    grant: Option<&str>,
    delegation: Option<&str>,
) -> Result<()> {
    if !store::agent_exists(&ctx.conn, agent)? {
        return Err(TrelaneError::msg(format!("unknown agent '{agent}'")));
    }
    store::expire_stale_delegations(&ctx.conn, &crypto::now_iso())?;
    let rel = domain::norm_rel(&ctx.root, path)?;
    if domain::is_hard_forbidden(&rel) {
        return Err(TrelaneError::msg(format!(
            "never claim hard-forbidden path '{rel}'"
        )));
    }

    let ttl = ttl.unwrap_or(ctx.config.claims.default_ttl_s);
    let now_ts = chrono::Utc::now().timestamp() as f64;

    let dom = store::get_domain(&ctx.conn, agent)?
        .ok_or_else(|| TrelaneError::msg(format!("agent '{agent}' not found")))?;
    let compiled = CompiledDomain::from_domain(&dom)?;
    let mine = compiled.is_writable(&rel);
    let others = owners_of(&ctx.conn, &rel, Some(agent))?;
    let mut existing = store::get_claim(&ctx.conn, &rel)?;
    if existing
        .as_ref()
        .is_some_and(|lease| lease.expires_at < now_ts)
    {
        store::delete_claim(&ctx.conn, &rel)?;
        existing = None;
    }

    let grant_delegation = match grant {
        Some(grant_id) => store::get_delegation_by_grant_message(&ctx.conn, grant_id)?,
        None => None,
    };
    let existing_delegation = existing
        .as_ref()
        .filter(|lease| lease.holder == agent)
        .and_then(|lease| lease.delegation_id.as_deref());
    let delegation_id = delegation
        .or(existing_delegation)
        .or_else(|| grant_delegation.as_ref().map(|d| d.id.as_str()));
    if let (Some(requested), Some(existing_id)) = (delegation, existing_delegation)
        && requested != existing_id
    {
        return Err(TrelaneError::msg(
            "a delegated lease can only be renewed with its original delegation",
        ));
    }

    let requires_delegation =
        (!mine && !others.is_empty()) || delegation_id.is_some() || existing_delegation.is_some();
    let authorized = if requires_delegation {
        let id = delegation_id.ok_or_else(|| {
            TrelaneError::msg(format!(
                "'{rel}' belongs to {}; an active --delegation is required",
                others.join(", ")
            ))
        })?;
        Some(authorize_delegated_claim(
            ctx, agent, id, grant, task, &rel,
        )?)
    } else {
        None
    };

    let requested_expiry = chrono::Utc::now()
        .checked_add_signed(chrono::Duration::seconds(ttl as i64))
        .ok_or_else(|| TrelaneError::msg("claim ttl is too large"))?;
    let effective_expiry = authorized
        .as_ref()
        .map(|(_, expiry)| requested_expiry.min(*expiry))
        .unwrap_or(requested_expiry);
    let expires_at = effective_expiry.timestamp() as f64;
    let expires_human = effective_expiry.format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let effective_task = authorized
        .as_ref()
        .map(|(d, _)| d.task_id.as_str())
        .or(task);
    let effective_grant = authorized
        .as_ref()
        .map(|(d, _)| d.grant_message.as_str())
        .or(grant);
    let effective_delegation = authorized.as_ref().map(|(d, _)| d.id.as_str());

    if let Some(lease) = existing {
        if lease.holder == agent {
            store::update_claim_renewal(
                &ctx.conn,
                &rel,
                effective_task,
                effective_grant,
                effective_delegation,
                expires_at,
                &expires_human,
            )?;
            println!("renewed lease on {rel} (until {expires_human})");
            return Ok(());
        }
        return Err(TrelaneError::msg(format!(
            "DENIED: {rel} is leased by {} until {}",
            lease.holder, lease.expires_human
        )));
    }

    let new_lease = Lease {
        path: rel.clone(),
        holder: agent.to_string(),
        task: effective_task.map(str::to_string),
        grant: effective_grant.map(str::to_string),
        delegation_id: effective_delegation.map(str::to_string),
        acquired_at: crypto::now_iso(),
        expires_at,
        expires_human,
        contested: !others.is_empty(),
    };

    match store::insert_claim(&ctx.conn, &new_lease) {
        Ok(true) => {
            let tag = if new_lease.contested {
                " (contested -- overlaps another domain; lease is mandatory)"
            } else {
                ""
            };
            println!("claimed {rel} for {agent}, ttl {ttl}s{tag}");
            Ok(())
        }
        Ok(false) => Err(TrelaneError::msg(format!(
            "DENIED: lost race for {rel}; re-check the lease before retrying"
        ))),
        Err(e) => Err(e),
    }
}

// --------------------------------------------------------------- release

pub fn cmd_release(ctx: &Context, agent: &str, path: &str, force: bool) -> Result<()> {
    if !store::agent_exists(&ctx.conn, agent)? {
        return Err(TrelaneError::msg(format!("unknown agent '{agent}'")));
    }
    let rel = domain::norm_rel(&ctx.root, path)?;
    match store::get_claim(&ctx.conn, &rel)? {
        None => {
            println!("(no lease on {rel})");
            Ok(())
        }
        Some(lease) => {
            if lease.holder != agent && !force {
                return Err(TrelaneError::msg(format!(
                    "{rel} is held by {}, not you (use --force only if reaping)",
                    lease.holder
                )));
            }
            store::delete_claim(&ctx.conn, &rel)?;
            println!("released {rel}");
            Ok(())
        }
    }
}

// ------------------------------------------------------------------ park

pub fn cmd_park(
    ctx: &Context,
    agent: &str,
    task: Option<&str>,
    wait_reply: &Option<String>,
    wait_claim: &Option<String>,
    waiting_on: &str,
    resume_hint: &str,
) -> Result<()> {
    if !store::agent_exists(&ctx.conn, agent)? {
        return Err(TrelaneError::msg(format!("unknown agent '{agent}'")));
    }
    if wait_reply.is_some() == wait_claim.is_some() {
        return Err(TrelaneError::msg(
            "specify exactly one of --wait-reply MSG_ID or --wait-claim PATH",
        ));
    }

    let (wait_type, wait_re, wait_path) = match (wait_reply, wait_claim) {
        (Some(re), None) => ("reply".to_string(), Some(re.clone()), None),
        (None, Some(p)) => {
            let rel = domain::norm_rel(&ctx.root, p)?;
            ("claim".to_string(), None, Some(rel))
        }
        _ => unreachable!(),
    };

    let task_id = task
        .map(|s| s.to_string())
        .unwrap_or_else(|| crypto::new_id("task"));
    let entry = ParkedTask {
        task: task_id.clone(),
        agent: agent.to_string(),
        wait_type: wait_type.clone(),
        wait_re,
        wait_path,
        waiting_on: waiting_on.to_string(),
        resume_hint: resume_hint.to_string(),
        created_at: crypto::now_iso(),
    };
    store::insert_parked_task(&ctx.conn, &entry)?;

    // Write park metadata for telemetry (read by cmd_unpark).
    let park_meta = ctx
        .trelane_dir()
        .join("agents")
        .join(agent)
        .join("park.json");
    if let Some(parent) = park_meta.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let park_data = serde_json::json!({
        "parked_at_ns": crate::telemetry::now_nanos(),
        "task_id": task_id,
        "wait_type": wait_type,
        "waiting_on": waiting_on,
    });
    let _ = std::fs::write(&park_meta, serde_json::to_string(&park_data)?);

    println!("{task_id}");
    Ok(())
}

// ---------------------------------------------------------------- unpark

pub fn cmd_unpark(ctx: &Context, task: &str) -> Result<()> {
    // Read park metadata for telemetry before deleting.
    let park_meta = ctx.trelane_dir().join("agents").join("park.json");

    store::delete_parked_task(&ctx.conn, task)?;

    // Record wait span if we have park metadata.
    if park_meta.exists()
        && let Ok(text) = std::fs::read_to_string(&park_meta)
        && let Ok(data) = serde_json::from_str::<serde_json::Value>(&text)
    {
        let parked_ns = data["parked_at_ns"].as_u64().unwrap_or(0);
        let agent = data.get("agent").and_then(|v| v.as_str()).unwrap_or("");
        let wait_type = data["wait_type"].as_str().unwrap_or("unknown");
        let waiting_on = data["waiting_on"].as_str().unwrap_or("unknown");
        let now_ns = crate::telemetry::now_nanos();

        // Find which agent's park.json this is by searching agents dir.
        let agents_dir = ctx.trelane_dir().join("agents");
        let mut found_agent = String::new();
        if let Ok(entries) = std::fs::read_dir(&agents_dir) {
            for entry in entries.flatten() {
                let candidate = entry.path().join("park.json");
                if candidate == park_meta {
                    found_agent = entry.file_name().to_string_lossy().to_string();
                    break;
                }
            }
        }
        let agent_name = if found_agent.is_empty() {
            agent
        } else {
            found_agent.as_str()
        };

        if !agent_name.is_empty()
            && let Ok(tracer) = crate::telemetry::Tracer::ephemeral(
                &ctx.trelane_dir(),
                &ctx.root.display().to_string(),
            )
        {
            let _ = tracer.record_agent_wait(
                agent_name, task, waiting_on, wait_type, parked_ns, now_ns, true,
            );
        }
        let _ = std::fs::remove_file(&park_meta);
    }

    println!("unparked {task}");
    Ok(())
}

// ---------------------------------------------------------------- status

pub fn cmd_status(ctx: &Context) -> Result<()> {
    let agents = store::list_agents(&ctx.conn)?;
    println!("project root : {}", ctx.root.display());
    let agents_str = if agents.is_empty() {
        "(none)".to_string()
    } else {
        agents.join(", ")
    };
    println!("agents       : {agents_str}");

    for ag in &agents {
        let n = store::get_unprocessed_messages(&ctx.conn, ag)?.len();
        let run = if is_running(&ctx.conn, ag)? {
            "RUNNING"
        } else {
            "stopped"
        };
        println!("  {ag:<16} {run:<8} inbox={n}");
        if let Some(dom) = store::get_domain(&ctx.conn, ag)? {
            println!("    writable   : {}", list_or_none(&dom.writable));
            println!(
                "    launcher   : {}",
                dom.launcher_agent
                    .unwrap_or_else(|| "(default)".to_string())
            );
            println!("    forbidden  : {}", list_or_none(&dom.forbidden_write));
        }
    }

    let session_agents = store::list_session_agents(&ctx.conn)?;
    if !session_agents.is_empty() {
        println!("session agents:");
        for (name, enabled, source) in &session_agents {
            let state = if *enabled { "enabled" } else { "disabled" };
            println!("  {name:<24} {state:<8} source={source}");
        }
    }

    // Concurrency: registered count and the simultaneous-execution ceiling are
    // shown as separate numbers so a swarm with more registered agents than
    // the limit is never mistaken for a stuck/idle swarm -- the extra agents
    // are simply waiting for a free slot (see `squire.max_concurrent`).
    let running_count = agents
        .iter()
        .filter(|a| is_running(&ctx.conn, a).unwrap_or(false))
        .count();
    let limit = ctx.config.squire.max_concurrent;
    println!(
        "concurrency  : {} registered / {} running / limit {} ({} slot(s) free)",
        agents.len(),
        running_count,
        limit,
        limit.saturating_sub(running_count),
    );

    let launch_targets = store::list_launch_targets(&ctx.conn)?;
    if !launch_targets.is_empty() {
        println!("launch targets:");
        for target in &launch_targets {
            println!(
                "  {:<16} adapter={} target={} tmux_target={} command={}",
                target.agent,
                target.adapter,
                target.target,
                target.tmux_target.as_deref().unwrap_or("(none)"),
                target.command
            );
        }
    }

    let parked = store::list_parked_tasks(&ctx.conn)?;
    println!("parked tasks : {}", parked.len());
    for e in &parked {
        let sat = if prompt::park_satisfied(&ctx.conn, e)? {
            "READY"
        } else {
            "waiting"
        };
        let wait = match e.wait_type.as_str() {
            "reply" => format!("reply to {}", e.wait_re.as_deref().unwrap_or("?")),
            "claim" => format!("claim on {}", e.wait_path.as_deref().unwrap_or("?")),
            _ => e.wait_type.clone(),
        };
        println!(
            "  {}  {} -> {} [{sat}] {wait}",
            e.task, e.agent, e.waiting_on
        );
    }

    let leases = store::list_claims(&ctx.conn)?;
    println!("claims       : {}", leases.len());
    let now_ts = chrono::Utc::now().timestamp() as f64;
    for l in &leases {
        let exp = if l.expires_at < now_ts {
            "EXPIRED".to_string()
        } else {
            l.expires_human.clone()
        };
        println!("  {}  held by {} until {exp}", l.path, l.holder);
    }

    let (_, cycle) = crate::squire::wait_graph(&ctx.conn)?;
    if let Some(cycle) = cycle {
        let mut display = cycle.clone();
        display.push(cycle[0].clone());
        println!("DEADLOCK     : cycle detected: {}", display.join(" -> "));
    } else {
        println!("deadlock     : none");
    }

    Ok(())
}

// ------------------------------------------------------------------ wake

pub fn cmd_wake(
    ctx: &Context,
    agent: &str,
    why: Option<&str>,
    launcher_override: Option<&str>,
) -> Result<()> {
    if !store::agent_exists(&ctx.conn, agent)? {
        return Err(TrelaneError::msg(format!("unknown agent '{agent}'")));
    }
    if !domain_launch_enabled(ctx, agent)? {
        return Err(TrelaneError::msg(format!(
            "agent '{agent}' is mapped to a disabled session agent/model"
        )));
    }
    if is_running(&ctx.conn, agent)? {
        return Err(TrelaneError::msg(format!("{agent} is already running")));
    }

    let reason = why.unwrap_or("manual wake");
    let prompt_text = prompt::compose_prompt(&ctx.conn, &ctx.root, agent, reason)?;
    let prompt_file = prompt::write_prompt_file(&ctx.trelane_dir(), agent, &prompt_text)?;

    // Record the git baseline before the run for diff computation on done.
    let baseline = git_dirty(&ctx.root);
    if let Some(ref dirty) = baseline {
        store::save_audit_baseline(&ctx.conn, agent, dirty)?;
    }

    // Write wake metadata for telemetry (read by cmd_done).
    let wake_meta = ctx
        .trelane_dir()
        .join("agents")
        .join(agent)
        .join("wake.json");
    if let Some(parent) = wake_meta.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let wake_data = serde_json::json!({
        "started_at_ns": crate::telemetry::now_nanos(),
        "started_at_iso": crypto::now_iso(),
        "reason": reason,
    });
    std::fs::write(&wake_meta, serde_json::to_string(&wake_data)?)?;

    if launcher_override.is_none()
        && let Some(target) = store::get_launch_target(&ctx.conn, agent)?
    {
        let resolved_command = if target.command.trim().is_empty() {
            launcher_command_for_agent(ctx, agent, &prompt_file, None)?
        } else {
            target
                .command
                .replace("{prompt_file}", &prompt_file.display().to_string())
                .replace("{agent}", agent)
                .replace("{root}", &ctx.root.display().to_string())
        };

        let command = command_for_launch_target(&LaunchTarget {
            command: resolved_command,
            ..target.clone()
        });

        if target.adapter == "tmux" {
            // Write a single launch script containing the splash AND the
            // agent command.  Sending two separate send-keys calls (splash
            // then launch) causes a race: the splash's `sleep` is still
            // running when the launch command arrives, so it gets lost.
            // One script = one send-keys = no race.
            let script_path = ctx
                .trelane_dir()
                .join("agents")
                .join(agent)
                .join("launch.sh");
            if let Some(parent) = script_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let logo = crate::logo::LOGO_SMALL.replace('\'', "'\"'\"'");
            let root_q = ctx.root.display().to_string().replace('\'', "'\"'\"'");
            let agent_q = agent.replace('\'', "'\"'\"'");
            let reason_q = reason.replace('\'', "'\"'\"'");
            std::fs::write(
                &script_path,
                format!(
                    "#!/bin/sh\nclear\nprintf '\\n\\n%s\\n  Agent   : %s\\n  Reason  : %s\\n  Project : %s\\n  Status  : launching...\\n\\n' '{logo}' '{agent_q}' '{reason_q}' '{root_q}'\nexec {command}\n",
                ),
            )?;

            launch_via_adapter(
                &target.adapter,
                &target.target,
                &format!(
                    "sh {}",
                    shell_single_quote(&script_path.display().to_string())
                ),
            )?;
        } else {
            launch_via_adapter(&target.adapter, &target.target, &command)?;
        }

        let inserted =
            store::insert_running_lock(&ctx.conn, agent, -1, &crypto::now_iso(), reason)?;
        if !inserted {
            eprintln!("warning: {agent} was already launched by another squire");
        }
        println!(
            "relaunched {agent} via {} target={} reason={reason}",
            target.adapter, target.target
        );
        return Ok(());
    }

    let cmd = launcher_command_for_agent(ctx, agent, &prompt_file, launcher_override)?;

    let log_dir = ctx.trelane_dir().join("agents").join(agent).join("logs");
    std::fs::create_dir_all(&log_dir)?;
    let log_name = format!("run-{}.log", crypto::new_id("r"));
    let log_path = log_dir.join(&log_name);
    let launch_script = format!(
        "({}) >> {} 2>&1 & printf '%s' $!",
        cmd,
        shell_double_quote(&log_path.display().to_string())
    );
    let output = Command::new("sh")
        .arg("-c")
        .arg(&launch_script)
        .current_dir(&ctx.root)
        .output()?;
    if !output.status.success() {
        return Err(TrelaneError::msg(format!(
            "launcher shell failed for {agent}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let pid_text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let pid = pid_text.parse::<i32>().map_err(|_| {
        TrelaneError::msg(format!("failed to parse launched pid from '{pid_text}'"))
    })?;

    let inserted = store::insert_running_lock(&ctx.conn, agent, pid, &crypto::now_iso(), reason)?;
    if !inserted {
        eprintln!("warning: {agent} was already launched by another squire");
    }
    println!("launched {agent} pid={pid} reason={reason}");
    Ok(())
}

pub fn cmd_set_launch_target(
    ctx: &Context,
    agent: &str,
    adapter: &str,
    target: &str,
    command: Option<&str>,
    tmux_target: Option<&str>,
) -> Result<()> {
    if !store::agent_exists(&ctx.conn, agent)? {
        return Err(TrelaneError::msg(format!("unknown agent '{agent}'")));
    }
    if !domain_launch_enabled(ctx, agent)? {
        return Err(TrelaneError::msg(format!(
            "agent '{agent}' is mapped to a disabled session agent/model"
        )));
    }
    let command = command.map(str::to_string).unwrap_or_default();
    store::upsert_launch_target(
        &ctx.conn,
        agent,
        adapter,
        target,
        &command,
        tmux_target,
        &crypto::now_iso(),
    )?;
    println!(
        "stored launch target for {agent}: {adapter} {target} tmux_target={}",
        tmux_target.unwrap_or("(none)")
    );
    Ok(())
}

pub fn cmd_relaunch(
    ctx: &Context,
    agent: &str,
    adapter: Option<&str>,
    target: Option<&str>,
    command: Option<&str>,
) -> Result<()> {
    if !store::agent_exists(&ctx.conn, agent)? {
        return Err(TrelaneError::msg(format!("unknown agent '{agent}'")));
    }
    if !domain_launch_enabled(ctx, agent)? {
        return Err(TrelaneError::msg(format!(
            "agent '{agent}' is mapped to a disabled session agent/model"
        )));
    }
    let prompt_text = prompt::compose_prompt(&ctx.conn, &ctx.root, agent, "manual relaunch")?;
    let prompt_file = prompt::write_prompt_file(&ctx.trelane_dir(), agent, &prompt_text)?;

    let stored = store::get_launch_target(&ctx.conn, agent)?;
    let adapter = adapter
        .map(str::to_string)
        .or_else(|| stored.as_ref().map(|t| t.adapter.clone()))
        .ok_or_else(|| TrelaneError::msg("missing --adapter and no stored launch target"))?;
    let target = target
        .map(str::to_string)
        .or_else(|| stored.as_ref().map(|t| t.target.clone()))
        .ok_or_else(|| TrelaneError::msg("missing --target and no stored launch target"))?;
    let command = command
        .map(str::to_string)
        .or_else(|| stored.as_ref().map(|t| t.command.clone()))
        .unwrap_or_default();

    let resolved_command = if command.trim().is_empty() {
        launcher_command_for_agent(ctx, agent, &prompt_file, None)?
    } else {
        command
            .replace("{prompt_file}", &prompt_file.display().to_string())
            .replace("{agent}", agent)
            .replace("{root}", &ctx.root.display().to_string())
    };

    let launch_target = LaunchTarget {
        agent: agent.to_string(),
        adapter: adapter.clone(),
        target: target.clone(),
        command: resolved_command,
        tmux_target: stored.as_ref().and_then(|t| t.tmux_target.clone()),
        updated_at: String::new(),
    };

    let command = command_for_launch_target(&launch_target);
    launch_via_adapter(&adapter, &target, &command)?;
    println!("relaunched {agent} via {adapter} target={target}");
    Ok(())
}

// ------------------------------------------------------------------ done

pub fn cmd_done(ctx: &Context, agent: &str) -> Result<()> {
    store::delete_running_lock(&ctx.conn, agent)?;

    // Record telemetry: read wake metadata, compute diff, emit span.
    let wake_meta = ctx
        .trelane_dir()
        .join("agents")
        .join(agent)
        .join("wake.json");
    if wake_meta.exists()
        && let Ok(text) = std::fs::read_to_string(&wake_meta)
        && let Ok(data) = serde_json::from_str::<serde_json::Value>(&text)
    {
        let started_ns = data["started_at_ns"].as_u64().unwrap_or(0);
        let reason = data["reason"].as_str().unwrap_or("unknown");
        let now_ns = crate::telemetry::now_nanos();
        let (files, added, removed) = crate::telemetry::git_diff_stats(&ctx.root);

        let msg_proc = crate::store::get_unprocessed_messages(&ctx.conn, agent)
            .unwrap_or_default()
            .len();
        let msg_sent = 0;

        if let Ok(tracer) =
            crate::telemetry::Tracer::ephemeral(&ctx.trelane_dir(), &ctx.root.display().to_string())
        {
            let _ = tracer.record_agent_run(
                agent, reason, started_ns, now_ns, files, added, removed, msg_proc, msg_sent,
                "done",
            );
        }
        let _ = std::fs::remove_file(&wake_meta);
    }

    println!("{agent} marked done");
    Ok(())
}

// ------------------------------------------------------------------ stub

pub fn cmd_stub(ctx: &Context, agent: &str) -> Result<()> {
    if !store::agent_exists(&ctx.conn, agent)? {
        return Err(TrelaneError::msg(format!("unknown agent '{agent}'")));
    }
    let secret = ctx.secret()?;
    println!("[stub:{agent}] awake");

    // 1. Resume satisfied parks
    let parked = store::list_parked_tasks_for_agent(&ctx.conn, agent)?;
    for e in &parked {
        if prompt::park_satisfied(&ctx.conn, e)? {
            println!(
                "[stub:{agent}] resuming parked task {} (hint: {})",
                e.task,
                if e.resume_hint.is_empty() {
                    "none"
                } else {
                    &e.resume_hint
                }
            );
            store::delete_parked_task(&ctx.conn, &e.task)?;
        }
    }

    // 2. Drain inbox
    let msgs = store::get_unprocessed_messages(&ctx.conn, agent)?;
    for m in &msgs {
        if !crypto::verify(&secret, m) {
            println!("[stub:{agent}] REJECTING unsigned/tampered {}", m.id);
            store::mark_processed(&ctx.conn, agent, &m.id, &crypto::now_iso())?;
            continue;
        }
        match m.msg_type.as_str() {
            "question" if m.from != "user" => {
                let mut reply = Message::new(
                    crypto::new_id("msg"),
                    agent.to_string(),
                    m.from.clone(),
                    "answer".to_string(),
                    "normal".to_string(),
                    format!("re: {}", m.subject),
                    "Stub answer: yes, proceed with the default approach.".to_string(),
                    Some(m.id.clone()),
                    None,
                    vec![],
                    crypto::now_iso(),
                );
                crypto::sign(&secret, &mut reply);
                store::insert_message(&ctx.conn, &reply)?;
                println!("[stub:{agent}] answered {} from {}", m.id, m.from);
            }
            "claim-request" => {
                let paths = m.paths.clone();
                let mut grant_msg = Message::new(
                    crypto::new_id("msg"),
                    agent.to_string(),
                    m.from.clone(),
                    "claim-grant".to_string(),
                    "normal".to_string(),
                    format!("granted: {}", m.subject),
                    "Stub grants this claim. Release when finished.".to_string(),
                    Some(m.id.clone()),
                    None,
                    paths,
                    crypto::now_iso(),
                );
                crypto::sign(&secret, &mut grant_msg);
                store::insert_message(&ctx.conn, &grant_msg)?;
                println!(
                    "[stub:{agent}] granted claim to {} for {:?}",
                    m.from, m.paths
                );
            }
            "claim-grant" => {
                for rel in &m.paths {
                    let full_path = ctx.root.join(rel);
                    cmd_claim(
                        ctx,
                        agent,
                        &full_path.to_string_lossy(),
                        Some(60),
                        None,
                        Some(&m.id),
                        None,
                    )?;
                    println!(
                        "[stub:{agent}] claimed {rel} using grant {}; pretending to edit; releasing",
                        m.id
                    );
                    cmd_release(ctx, agent, &full_path.to_string_lossy(), false)?;
                }
            }
            "info" if m.subject.starts_with("deadlock") => {
                for e in &store::list_parked_tasks_for_agent(&ctx.conn, agent)? {
                    if e.waiting_on == m.from {
                        println!(
                            "[stub:{agent}] counterpart broke deadlock; unparking {}",
                            e.task
                        );
                        store::delete_parked_task(&ctx.conn, &e.task)?;
                    }
                }
            }
            _ => {}
        }
        store::mark_processed(&ctx.conn, agent, &m.id, &crypto::now_iso())?;
    }

    // 3. Deadlock breaker
    if msgs.is_empty() {
        let stuck: Vec<_> = store::list_parked_tasks_for_agent(&ctx.conn, agent)?
            .into_iter()
            .filter(|e| !prompt::park_satisfied(&ctx.conn, e).unwrap_or(false))
            .collect();
        for e in &stuck {
            let other = &e.waiting_on;
            println!(
                "[stub:{agent}] deadlock breaker: unparking {}, proceeding on documented assumption, notifying {other}",
                e.task
            );
            store::delete_parked_task(&ctx.conn, &e.task)?;
            if other != "user" {
                let mut notify = Message::new(
                    crypto::new_id("msg"),
                    agent.to_string(),
                    other.clone(),
                    "info".to_string(),
                    "normal".to_string(),
                    "deadlock broken by counterpart".to_string(),
                    format!(
                        "I was designated deadlock breaker for the cycle involving us. I unparked '{}' and proceeded assuming: default interface, no breaking changes. Object via a new question if wrong.",
                        e.task
                    ),
                    None,
                    None,
                    vec![],
                    crypto::now_iso(),
                );
                crypto::sign(&secret, &mut notify);
                store::insert_message(&ctx.conn, &notify)?;
            }
        }
    }

    // 4. Done
    store::delete_running_lock(&ctx.conn, agent)?;
    println!("[stub:{agent}] slice complete, exiting");
    Ok(())
}

// ----------------------------------------------------------------- audit

pub fn cmd_audit(ctx: &Context, agent: &str) -> Result<()> {
    if !store::agent_exists(&ctx.conn, agent)? {
        return Err(TrelaneError::msg(format!("unknown agent '{agent}'")));
    }
    let dirty = match git_dirty(&ctx.root) {
        Some(d) => d,
        None => {
            println!("audit skipped: git unavailable or not a repository");
            return Ok(());
        }
    };

    let baseline = store::get_audit_baseline(&ctx.conn, agent)?.unwrap_or_default();
    let dom = store::get_domain(&ctx.conn, agent)?
        .ok_or_else(|| TrelaneError::msg(format!("agent '{agent}' not found")))?;
    let compiled = CompiledDomain::from_domain(&dom)?;

    let violations: Vec<String> = dirty
        .iter()
        .filter(|(rel, hash)| baseline.get(*rel) != Some(*hash) && !compiled.is_writable(rel))
        .map(|(rel, _)| rel.clone())
        .collect();

    if violations.is_empty() {
        println!("audit ok: no out-of-domain files dirtied during this run");
        return Ok(());
    }

    let viol = Violation {
        id: crypto::new_id("viol"),
        agent: agent.to_string(),
        paths: violations.clone(),
        at: crypto::now_iso(),
    };
    store::insert_violation(&ctx.conn, &viol)?;

    println!("AUDIT FAIL: {agent} dirtied files outside its domain this run:");
    for v in &violations {
        println!("  - {v}");
    }
    println!("Revert these changes or message the owning agent with a handoff.");
    std::process::exit(1);
}

// ---------------------------------------------------------- assistance C2

pub fn cmd_help(ctx: &Context, action: &crate::cli::HelpAction) -> Result<()> {
    use crate::cli::HelpAction;
    store::expire_stale_delegations(&ctx.conn, &crypto::now_iso())?;
    match action {
        HelpAction::Request {
            from,
            to,
            task,
            paths,
            need,
        } => cmd_help_request(ctx, from, to, task, paths, need),
        HelpAction::Offer {
            from,
            to,
            task,
            paths,
            plan,
            deliverable,
            allowed_ops,
        } => cmd_help_offer(ctx, from, to, task, paths, plan, deliverable, allowed_ops),
        HelpAction::Accept {
            id,
            by,
            paths,
            allowed_ops,
            ttl,
        } => cmd_help_accept(ctx, id, by, paths, allowed_ops, *ttl),
        HelpAction::Deny { id, by, reason } => cmd_help_deny(ctx, id, by, reason),
        HelpAction::Revoke { id, by, reason } => cmd_help_revoke(ctx, id, by, reason),
    }
}

fn require_open_assistable_task(ctx: &Context, task_id: &str, owner: &str) -> Result<Task> {
    let task = store::get_task(&ctx.conn, task_id)?
        .ok_or_else(|| TrelaneError::msg(format!("no task '{task_id}'")))?;
    if task.owner_agent != owner {
        return Err(TrelaneError::msg(format!(
            "task '{task_id}' is owned by '{}', not '{owner}'",
            task.owner_agent
        )));
    }
    if task.state.is_terminal() {
        return Err(TrelaneError::msg(format!(
            "task '{task_id}' is terminal ({})",
            task.state.as_str()
        )));
    }
    if task.assist_policy != AssistPolicy::Open {
        return Err(TrelaneError::msg(format!(
            "task '{task_id}' does not accept assistance"
        )));
    }
    Ok(task)
}

#[allow(clippy::too_many_arguments)]
fn signed_protocol_message(
    ctx: &Context,
    from: &str,
    to: &str,
    msg_type: &str,
    subject: String,
    body: String,
    re: Option<String>,
    task: Option<String>,
    paths: Vec<String>,
) -> Result<Message> {
    let mut msg = Message::new(
        crypto::new_id("msg"),
        from.to_string(),
        to.to_string(),
        msg_type.to_string(),
        "normal".to_string(),
        subject,
        body,
        re,
        task,
        paths,
        crypto::now_iso(),
    );
    crypto::sign(&ctx.secret()?, &mut msg);
    Ok(msg)
}

fn cmd_help_request(
    ctx: &Context,
    owner: &str,
    helper: &str,
    task_id: &str,
    paths: &[String],
    need: &str,
) -> Result<()> {
    if owner == helper {
        return Err(TrelaneError::msg(
            "owner and helper must be different agents",
        ));
    }
    if !store::agent_exists(&ctx.conn, helper)? {
        return Err(TrelaneError::msg(format!(
            "unknown helper agent '{helper}'"
        )));
    }
    let task = require_open_assistable_task(ctx, task_id, owner)?;
    let requested = if paths.is_empty() {
        task.path_scope.clone()
    } else {
        paths.to_vec()
    };
    let scope = validate_delegable_scope(ctx, &task, owner, &requested)?;
    let msg = signed_protocol_message(
        ctx,
        owner,
        helper,
        "help-request",
        format!("help requested for task {task_id}"),
        serde_json::json!({"need": need}).to_string(),
        None,
        Some(task_id.to_string()),
        scope,
    )?;
    store::insert_message(&ctx.conn, &msg)?;
    println!("{}", msg.id);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_help_offer(
    ctx: &Context,
    helper: &str,
    owner: &str,
    task_id: &str,
    paths: &[String],
    plan: &str,
    deliverable: &str,
    allowed_ops: &[String],
) -> Result<()> {
    if helper == owner {
        return Err(TrelaneError::msg(
            "owner and helper must be different agents",
        ));
    }
    if !store::agent_exists(&ctx.conn, helper)? {
        return Err(TrelaneError::msg(format!(
            "unknown helper agent '{helper}'"
        )));
    }
    let task = require_open_assistable_task(ctx, task_id, owner)?;
    let proposed = if paths.is_empty() {
        task.path_scope.clone()
    } else {
        paths.to_vec()
    };
    let scope = validate_delegable_scope(ctx, &task, owner, &proposed)?;
    let ops = if allowed_ops.is_empty() {
        vec!["write".to_string()]
    } else {
        allowed_ops.to_vec()
    };
    if ops.iter().any(|op| op.trim().is_empty()) {
        return Err(TrelaneError::msg("allowed operations cannot be empty"));
    }
    let id = crypto::new_id("del");
    let body = serde_json::json!({
        "delegation_id": id,
        "plan": plan,
        "deliverable": deliverable,
        "allowed_ops": ops,
    })
    .to_string();
    let msg = signed_protocol_message(
        ctx,
        helper,
        owner,
        "help-offer",
        format!("help offered for task {task_id}"),
        body,
        None,
        Some(task_id.to_string()),
        scope.clone(),
    )?;
    let base_revision = git_head(&ctx.root).ok();
    let delegation = Delegation {
        id: id.clone(),
        task_id: task_id.to_string(),
        owner_agent: owner.to_string(),
        helper_agent: helper.to_string(),
        scope,
        allowed_ops: ops,
        constraints_json: serde_json::json!({
            "plan": plan,
            "deliverable": deliverable,
        })
        .to_string(),
        base_revision,
        offer_message: msg.id.clone(),
        grant_message: String::new(),
        issued_at: crypto::now_iso(),
        expires_at: None,
        status: DelegationStatus::Offered,
    };
    store::insert_offer(&ctx.conn, &delegation, &msg)?;
    // C3: record the backlog fingerprint at offer time so the scheduler does
    // not keep waking this helper for the same unchanged backlog.
    let assistable = store::list_assistable_tasks(&ctx.conn, helper, &crypto::now_iso())?;
    let fingerprint = store::assist_backlog_fingerprint(&assistable);
    let _ = store::record_offer_fingerprint(&ctx.conn, helper, &fingerprint, &id, &crypto::now_iso());
    println!("{id}");
    Ok(())
}

fn cmd_help_accept(
    ctx: &Context,
    id: &str,
    owner: &str,
    paths: &[String],
    allowed_ops: &[String],
    ttl: u64,
) -> Result<()> {
    if ttl == 0 {
        return Err(TrelaneError::msg(
            "delegation ttl must be greater than zero",
        ));
    }
    let offered = store::get_delegation(&ctx.conn, id)?
        .ok_or_else(|| TrelaneError::msg(format!("no help offer '{id}'")))?;
    if offered.status != DelegationStatus::Offered {
        return Err(TrelaneError::msg(format!(
            "help offer '{id}' is {}, not offered",
            offered.status.as_str()
        )));
    }
    if offered.owner_agent != owner {
        return Err(TrelaneError::msg(format!(
            "only owner '{}' may accept this offer",
            offered.owner_agent
        )));
    }
    let task = require_open_assistable_task(ctx, &offered.task_id, owner)?;
    let narrowed = if paths.is_empty() {
        offered.scope.clone()
    } else {
        paths.to_vec()
    };
    let scope = validate_delegable_scope(ctx, &task, owner, &narrowed)?;
    if !scope_is_subset(&scope, &offered.scope)? {
        return Err(TrelaneError::msg(
            "accepted path scope is not a provable subset of the offer",
        ));
    }
    let ops = if allowed_ops.is_empty() {
        offered.allowed_ops.clone()
    } else {
        allowed_ops.to_vec()
    };
    if ops.is_empty()
        || ops
            .iter()
            .any(|op| !offered.allowed_ops.iter().any(|offered| offered == op))
    {
        return Err(TrelaneError::msg(
            "accepted operations must be a non-empty subset of offered operations",
        ));
    }
    let now = crypto::now_iso();
    let expires_at = chrono::Utc::now()
        .checked_add_signed(chrono::Duration::seconds(ttl as i64))
        .ok_or_else(|| TrelaneError::msg("delegation ttl is too large"))?
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let grant = signed_protocol_message(
        ctx,
        owner,
        &offered.helper_agent,
        "help-accept",
        format!("delegation {id} accepted"),
        serde_json::json!({
            "delegation_id": id,
            "allowed_ops": ops,
            "expires_at": expires_at,
        })
        .to_string(),
        Some(offered.offer_message.clone()),
        Some(offered.task_id.clone()),
        scope.clone(),
    )?;
    store::activate_delegation_and_assign(&ctx.conn, id, &scope, &ops, &expires_at, &grant, &now)?;
    // C3: acceptance clears any rejection backoff between this owner/helper pair.
    let _ = store::clear_rejection_backoff(&ctx.conn, &offered.helper_agent, owner);
    println!("accepted {id} until {expires_at} (grant {})", grant.id);
    Ok(())
}

fn cmd_help_deny(ctx: &Context, id: &str, owner: &str, reason: &str) -> Result<()> {
    if !store::agent_exists(&ctx.conn, owner)? {
        return Err(TrelaneError::msg(format!("unknown owner agent '{owner}'")));
    }
    let offered = store::get_delegation(&ctx.conn, id)?
        .ok_or_else(|| TrelaneError::msg(format!("no help offer '{id}'")))?;
    if offered.owner_agent != owner {
        return Err(TrelaneError::msg(format!(
            "only owner '{}' may deny this offer",
            offered.owner_agent
        )));
    }
    let msg = signed_protocol_message(
        ctx,
        owner,
        &offered.helper_agent,
        "help-deny",
        format!("help offer {id} denied"),
        serde_json::json!({"delegation_id": id, "reason": reason}).to_string(),
        Some(offered.offer_message),
        Some(offered.task_id),
        vec![],
    )?;
    if !store::reject_offer_with_message(&ctx.conn, id, &msg, &crypto::now_iso())? {
        return Err(TrelaneError::msg(format!(
            "help offer '{id}' is no longer pending"
        )));
    }
    // C3: exponential rejection backoff so a denied offer does not immediately
    // re-fire on the next tick.
    let _ = store::record_rejection_backoff(
        &ctx.conn,
        &offered.helper_agent,
        owner,
        &crypto::now_iso(),
    );
    println!("denied {id}");
    Ok(())
}

fn cmd_help_revoke(ctx: &Context, id: &str, owner: &str, reason: &str) -> Result<()> {
    if !store::agent_exists(&ctx.conn, owner)? {
        return Err(TrelaneError::msg(format!("unknown owner agent '{owner}'")));
    }
    let delegation = store::get_delegation(&ctx.conn, id)?
        .ok_or_else(|| TrelaneError::msg(format!("no delegation '{id}'")))?;
    if delegation.owner_agent != owner {
        return Err(TrelaneError::msg(format!(
            "only owner '{}' may revoke this delegation",
            delegation.owner_agent
        )));
    }
    let msg = signed_protocol_message(
        ctx,
        owner,
        &delegation.helper_agent,
        "help-revoke",
        format!("delegation {id} revoked"),
        serde_json::json!({"delegation_id": id, "reason": reason}).to_string(),
        Some(delegation.grant_message),
        Some(delegation.task_id),
        vec![],
    )?;
    if !store::revoke_delegation_with_message(&ctx.conn, id, &msg, &crypto::now_iso())? {
        return Err(TrelaneError::msg(format!(
            "delegation '{id}' is already terminal"
        )));
    }
    println!("revoked {id}");
    Ok(())
}

// ------------------------------------------------------------- work ledger

/// `trelane work ...` entry point (C1). Dispatches the ledger subcommands.
pub fn cmd_work(ctx: &Context, action: &crate::cli::WorkAction) -> Result<()> {
    use crate::cli::WorkAction;
    match action {
        WorkAction::List {
            owner,
            state,
            json,
            assistable,
            agent,
        } => cmd_work_list(
            ctx,
            owner.as_deref(),
            state.as_deref(),
            *json,
            *assistable,
            agent.as_deref(),
        ),
        WorkAction::Show { id } => cmd_work_show(ctx, id),
        WorkAction::Add {
            owner,
            subject,
            body,
            priority,
            paths,
            acceptance,
            blocked_by,
            parallelism,
            assist,
        } => cmd_work_add(
            ctx,
            owner,
            subject,
            body,
            priority,
            paths,
            acceptance,
            blocked_by,
            *parallelism,
            assist,
        ),
        WorkAction::Submit {
            task,
            by,
            delegation,
            commit,
            summary,
            tests,
        } => cmd_work_submit(ctx, task, by, delegation, commit, summary, tests),
        WorkAction::Review {
            task,
            by,
            delegation,
            accept,
            request_changes,
            reject,
            notes,
        } => cmd_work_review(
            ctx,
            task,
            by,
            delegation,
            *accept,
            *request_changes,
            *reject,
            notes,
        ),
    }
}

fn cmd_work_list(
    ctx: &Context,
    owner: Option<&str>,
    state: Option<&str>,
    json: bool,
    assistable: bool,
    agent: Option<&str>,
) -> Result<()> {
    let mut tasks = match owner {
        Some(o) => store::list_tasks_for_owner(&ctx.conn, o)?,
        None => store::list_tasks(&ctx.conn)?,
    };
    if let Some(s) = state {
        let want = TaskState::parse(s).ok_or_else(|| {
            TrelaneError::msg(format!(
                "unknown task state '{s}'. Known: {}",
                TASK_STATES.join(", ")
            ))
        })?;
        tasks.retain(|t| t.state == want);
    }
    if assistable {
        if let Some(helper) = agent
            && !store::agent_exists(&ctx.conn, helper)?
        {
            return Err(TrelaneError::msg(format!(
                "unknown prospective helper agent '{helper}'"
            )));
        }
        let helper_delegations = match agent {
            Some(helper) => store::list_delegations_for_helper(&ctx.conn, helper, None)?,
            None => vec![],
        };
        let mut filtered = Vec::new();
        for task in tasks {
            let active_helpers = store::list_assignments_for_task(&ctx.conn, &task.id)?
                .into_iter()
                .filter(|assignment| {
                    assignment.role == TaskRole::Helper
                        && matches!(assignment.state.as_str(), "active" | "submitted")
                })
                .count();
            let helper_already_involved = helper_delegations.iter().any(|delegation| {
                delegation.task_id == task.id
                    && matches!(
                        delegation.status,
                        DelegationStatus::Offered
                            | DelegationStatus::Active
                            | DelegationStatus::Submitted
                    )
            });
            if !task.state.is_terminal()
                && task.assist_policy == AssistPolicy::Open
                && agent.is_none_or(|helper| task.owner_agent != helper)
                && active_helpers < task.desired_parallelism as usize
                && !helper_already_involved
            {
                filtered.push(task);
            }
        }
        tasks = filtered;
    } else if agent.is_some() {
        return Err(TrelaneError::msg("--agent requires --assistable"));
    }
    if json {
        let arr: Vec<serde_json::Value> = tasks
            .iter()
            .map(|t| {
                serde_json::json!({
                    "id": t.id,
                    "owner": t.owner_agent,
                    "state": t.state.as_str(),
                    "priority": t.priority,
                    "subject": t.subject,
                    "blocked_by": t.blocked_by,
                    "path_scope": t.path_scope,
                    "assist_policy": t.assist_policy.as_str(),
                    "desired_parallelism": t.desired_parallelism,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr)?);
        return Ok(());
    }
    if tasks.is_empty() {
        println!("no tasks");
        return Ok(());
    }
    println!(
        "{:<20} {:<12} {:<9} {:<8} {}",
        "id", "owner", "state", "priority", "subject"
    );
    for t in &tasks {
        println!(
            "{:<20} {:<12} {:<9} {:<8} {}",
            t.id,
            t.owner_agent,
            t.state.as_str(),
            t.priority,
            t.subject
        );
    }
    Ok(())
}

fn cmd_work_show(ctx: &Context, id: &str) -> Result<()> {
    let task = store::get_task(&ctx.conn, id)?
        .ok_or_else(|| TrelaneError::msg(format!("no task '{id}'")))?;
    println!("id             : {}", task.id);
    println!("owner          : {}", task.owner_agent);
    println!("domain         : {}", task.domain);
    if let Some(p) = &task.parent_task {
        println!("parent         : {p}");
    }
    println!("state          : {}", task.state.as_str());
    println!("priority       : {}", task.priority);
    println!("assist policy  : {}", task.assist_policy.as_str());
    println!("parallelism    : {}", task.desired_parallelism);
    println!("subject        : {}", task.subject);
    if !task.body.is_empty() {
        println!("body           : {}", task.body);
    }
    println!("path scope     : {}", list_or_none(&task.path_scope));
    println!("acceptance     : {}", list_or_none(&task.acceptance));
    println!("blocked by     : {}", list_or_none(&task.blocked_by));
    println!("created        : {}", task.created_at);
    println!("updated        : {}", task.updated_at);

    let assignments = store::list_assignments_for_task(&ctx.conn, id)?;
    if !assignments.is_empty() {
        println!("assignments    :");
        for a in &assignments {
            println!("  {} [{}] state={}", a.agent, a.role.as_str(), a.state);
        }
    }
    let delegations = store::list_delegations_for_task(&ctx.conn, id)?;
    if !delegations.is_empty() {
        println!("delegations    :");
        for d in &delegations {
            println!(
                "  {} -> {} [{}] scope={}",
                d.owner_agent,
                d.helper_agent,
                d.status.as_str(),
                list_or_none(&d.scope)
            );
        }
    }
    let submissions = store::list_submissions_for_task(&ctx.conn, id)?;
    if !submissions.is_empty() {
        println!("submissions    :");
        for submission in &submissions {
            println!(
                "  {} by {} [{}] commit={} paths={}",
                submission.id,
                submission.helper_agent,
                submission.status,
                submission.commit_ref,
                list_or_none(&submission.changed_paths)
            );
        }
    }
    let reviews = store::list_reviews_for_task(&ctx.conn, id)?;
    if !reviews.is_empty() {
        println!("reviews        :");
        for r in &reviews {
            println!(
                "  {} -> {} {}",
                r.reviewer_agent,
                r.decision.as_str(),
                r.notes
            );
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_work_add(
    ctx: &Context,
    owner: &str,
    subject: &str,
    body: &str,
    priority: &str,
    paths: &[String],
    acceptance: &[String],
    blocked_by: &[String],
    parallelism: u32,
    assist: &str,
) -> Result<()> {
    if !store::agent_exists(&ctx.conn, owner)? {
        return Err(TrelaneError::msg(format!("unknown owner agent '{owner}'")));
    }
    if !URGENCIES.contains(&priority) {
        return Err(TrelaneError::msg(format!(
            "unknown priority '{priority}'. Known: {}",
            URGENCIES.join(", ")
        )));
    }
    let assist_policy = AssistPolicy::parse(assist).ok_or_else(|| {
        TrelaneError::msg(format!(
            "unknown assist policy '{assist}'. Known: open, solo"
        ))
    })?;
    // Dependencies must reference tasks that actually exist, so readiness can
    // be evaluated meaningfully.
    for dep in blocked_by {
        if store::get_task(&ctx.conn, dep)?.is_none() {
            return Err(TrelaneError::msg(format!(
                "blocked-by task '{dep}' does not exist"
            )));
        }
    }
    let now = crate::crypto::now_iso();
    let task = Task {
        id: crate::crypto::new_id("task"),
        owner_agent: owner.to_string(),
        domain: owner.to_string(),
        parent_task: None,
        subject: subject.to_string(),
        body: body.to_string(),
        state: TaskState::Ready,
        priority: priority.to_string(),
        assist_policy,
        desired_parallelism: parallelism.max(1),
        path_scope: paths.to_vec(),
        acceptance: acceptance.to_vec(),
        blocked_by: blocked_by.to_vec(),
        created_at: now.clone(),
        updated_at: now,
    };
    store::insert_task(&ctx.conn, &task)?;
    println!(
        "created task {} (owner {}, state ready)",
        task.id, task.owner_agent
    );
    Ok(())
}

fn git_output(root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(|e| TrelaneError::msg(format!("Git is required for work submission: {e}")))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(TrelaneError::msg(format!(
            "Git validation failed for `git {}`: {}",
            args.join(" "),
            if detail.is_empty() {
                "command failed".to_string()
            } else {
                detail
            }
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn ensure_git_repository(root: &Path) -> Result<()> {
    let value = git_output(root, &["rev-parse", "--is-inside-work-tree"])?;
    if value != "true" {
        return Err(TrelaneError::msg(
            "work submit requires a Git work tree; validation cannot be skipped",
        ));
    }
    Ok(())
}

fn git_head(root: &Path) -> Result<String> {
    ensure_git_repository(root)?;
    git_output(root, &["rev-parse", "--verify", "HEAD^{commit}"])
}

fn resolve_git_commit(root: &Path, reference: &str) -> Result<String> {
    git_output(
        root,
        &["rev-parse", "--verify", &format!("{reference}^{{commit}}")],
    )
}

fn git_is_ancestor(root: &Path, ancestor: &str, descendant: &str) -> Result<bool> {
    let status = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["merge-base", "--is-ancestor", ancestor, descendant])
        .status()
        .map_err(|e| TrelaneError::msg(format!("Git is required for work submission: {e}")))?;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(TrelaneError::msg(
            "Git could not compare the submission base and commit",
        )),
    }
}

fn git_changed_paths(root: &Path, base: &str, commit: &str) -> Result<Vec<String>> {
    let range = format!("{base}..{commit}");
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "diff",
            "--name-only",
            "-z",
            "--no-renames",
            "--diff-filter=ACDMRTUXB",
            &range,
            "--",
        ])
        .output()
        .map_err(|e| TrelaneError::msg(format!("Git is required for work submission: {e}")))?;
    if !output.status.success() {
        return Err(TrelaneError::msg(format!(
            "Git could not enumerate submission paths: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| {
            std::str::from_utf8(path)
                .map(|path| path.replace(std::path::MAIN_SEPARATOR, "/"))
                .map_err(|_| TrelaneError::msg("Git reported a non-UTF-8 changed path"))
        })
        .collect()
}

fn invalidate_stale_delegation(ctx: &Context, delegation: &Delegation, reason: &str) -> Result<()> {
    let msg = signed_protocol_message(
        ctx,
        "system",
        &delegation.helper_agent,
        "help-revoke",
        format!("delegation {} invalidated", delegation.id),
        serde_json::json!({
            "delegation_id": delegation.id,
            "reason": reason,
        })
        .to_string(),
        Some(delegation.grant_message.clone()),
        Some(delegation.task_id.clone()),
        vec![],
    )?;
    store::revoke_delegation_with_message(&ctx.conn, &delegation.id, &msg, &crypto::now_iso())?;
    Ok(())
}

fn validate_submission_paths(
    task: &Task,
    delegation: &Delegation,
    owner_domain: &Domain,
    paths: &[String],
) -> Result<()> {
    let compiled_owner = CompiledDomain::from_domain(owner_domain)?;
    for path in paths {
        if domain::is_hard_forbidden(path) {
            return Err(TrelaneError::msg(format!(
                "submission changes hard-forbidden path '{path}'"
            )));
        }
        if !domain::scope_covers_path(&task.path_scope, path)? {
            return Err(TrelaneError::msg(format!(
                "submission path '{path}' is outside task scope"
            )));
        }
        if !domain::scope_covers_path(&delegation.scope, path)? {
            return Err(TrelaneError::msg(format!(
                "submission path '{path}' is outside delegation scope"
            )));
        }
        if !compiled_owner.is_writable(path) {
            return Err(TrelaneError::msg(format!(
                "submission path '{path}' is no longer writable by owner '{}'",
                delegation.owner_agent
            )));
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_work_submit(
    ctx: &Context,
    task_id: &str,
    helper: &str,
    delegation_id: &str,
    commit_ref: &str,
    summary: &str,
    tests: &str,
) -> Result<()> {
    store::expire_stale_delegations(&ctx.conn, &crypto::now_iso())?;
    if !store::agent_exists(&ctx.conn, helper)? {
        return Err(TrelaneError::msg(format!(
            "unknown helper agent '{helper}'"
        )));
    }
    let task = store::get_task(&ctx.conn, task_id)?
        .ok_or_else(|| TrelaneError::msg(format!("no task '{task_id}'")))?;
    let delegation = store::get_delegation(&ctx.conn, delegation_id)?
        .ok_or_else(|| TrelaneError::msg(format!("no delegation '{delegation_id}'")))?;
    if delegation.task_id != task.id
        || delegation.helper_agent != helper
        || delegation.status != DelegationStatus::Active
    {
        return Err(TrelaneError::msg(
            "submission requires the matching helper's active delegation for this task",
        ));
    }
    if delegation_expiry(&delegation)? <= chrono::Utc::now() {
        return Err(TrelaneError::msg("submission delegation has expired"));
    }
    if !grant_message_verifies(ctx, &delegation, None)? {
        return Err(TrelaneError::msg(
            "submission delegation has no valid signed help-accept grant",
        ));
    }

    ensure_git_repository(&ctx.root)?;
    let commit = resolve_git_commit(&ctx.root, commit_ref)?;
    let base_ref = delegation
        .base_revision
        .as_deref()
        .ok_or_else(|| TrelaneError::msg("delegation has no validated Git base revision"))?;
    let base = match resolve_git_commit(&ctx.root, base_ref) {
        Ok(base) => base,
        Err(error) => {
            invalidate_stale_delegation(ctx, &delegation, "base revision no longer exists")?;
            return Err(error);
        }
    };
    if !git_is_ancestor(&ctx.root, &base, &commit)? {
        invalidate_stale_delegation(
            ctx,
            &delegation,
            "submission is not descended from its grant base",
        )?;
        return Err(TrelaneError::msg(
            "submission commit is not descended from the delegation base",
        ));
    }
    let current_head = git_head(&ctx.root)?;
    if !git_is_ancestor(&ctx.root, &current_head, &commit)? {
        invalidate_stale_delegation(ctx, &delegation, "base is stale relative to current HEAD")?;
        return Err(TrelaneError::msg(
            "delegation base is stale relative to current HEAD; rebase and resubmit",
        ));
    }
    let changed_paths = git_changed_paths(&ctx.root, &base, &commit)?;
    let owner_domain = store::get_domain(&ctx.conn, &delegation.owner_agent)?
        .ok_or_else(|| TrelaneError::msg("delegation owner no longer exists"))?;
    validate_submission_paths(&task, &delegation, &owner_domain, &changed_paths)?;

    let submission_id = crypto::new_id("sub");
    let message = signed_protocol_message(
        ctx,
        helper,
        &delegation.owner_agent,
        "submission",
        format!("submission for task {task_id}"),
        serde_json::json!({
            "submission_id": submission_id,
            "delegation_id": delegation_id,
            "commit": commit,
            "base_revision": base,
            "summary": summary,
            "tests": tests,
        })
        .to_string(),
        Some(delegation.grant_message.clone()),
        Some(task_id.to_string()),
        changed_paths.clone(),
    )?;
    let submission = TaskSubmission {
        id: submission_id.clone(),
        task_id: task_id.to_string(),
        delegation_id: delegation_id.to_string(),
        helper_agent: helper.to_string(),
        commit_ref: commit,
        base_revision: base,
        summary: summary.to_string(),
        tests: tests.to_string(),
        changed_paths,
        message_id: message.id.clone(),
        status: "pending".to_string(),
        created_at: crypto::now_iso(),
        reviewed_at: None,
    };
    store::record_submission(&ctx.conn, &submission, &message)?;
    println!("{submission_id}");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_work_review(
    ctx: &Context,
    task_id: &str,
    reviewer: &str,
    delegation_id: &str,
    accept: bool,
    request_changes: bool,
    reject: bool,
    notes: &str,
) -> Result<()> {
    let choices = [accept, request_changes, reject]
        .into_iter()
        .filter(|selected| *selected)
        .count();
    if choices != 1 {
        return Err(TrelaneError::msg(
            "specify exactly one of --accept, --request-changes, or --reject",
        ));
    }
    let decision = if accept {
        ReviewDecision::Accept
    } else if request_changes {
        ReviewDecision::RequestChanges
    } else {
        ReviewDecision::Reject
    };
    if !store::agent_exists(&ctx.conn, reviewer)? {
        return Err(TrelaneError::msg(format!(
            "unknown reviewer agent '{reviewer}'"
        )));
    }
    let task = store::get_task(&ctx.conn, task_id)?
        .ok_or_else(|| TrelaneError::msg(format!("no task '{task_id}'")))?;
    let reviewer_is_assigned = store::list_assignments_for_task(&ctx.conn, task_id)?
        .iter()
        .any(|assignment| assignment.agent == reviewer && assignment.role == TaskRole::Reviewer);
    if task.owner_agent != reviewer && !reviewer_is_assigned {
        return Err(TrelaneError::msg(format!(
            "'{reviewer}' is neither task owner nor designated reviewer"
        )));
    }
    let delegation = store::get_delegation(&ctx.conn, delegation_id)?
        .ok_or_else(|| TrelaneError::msg(format!("no delegation '{delegation_id}'")))?;
    if delegation.task_id != task.id || delegation.status != DelegationStatus::Submitted {
        return Err(TrelaneError::msg(
            "delegation is not awaiting review for this task",
        ));
    }
    if decision == ReviewDecision::RequestChanges
        && delegation_expiry(&delegation)? <= chrono::Utc::now()
    {
        return Err(TrelaneError::msg(
            "cannot request changes: delegation has expired; accept or reject the submission",
        ));
    }
    let submission = store::latest_submission_for_delegation(&ctx.conn, task_id, delegation_id)?
        .filter(|submission| submission.status == "pending")
        .ok_or_else(|| TrelaneError::msg("no pending submission for this delegation"))?;
    let now = crypto::now_iso();
    let review = TaskReview {
        id: crypto::new_id("rev"),
        task_id: task_id.to_string(),
        delegation_id: Some(delegation_id.to_string()),
        reviewer_agent: reviewer.to_string(),
        submission_ref: submission.id.clone(),
        decision,
        notes: notes.to_string(),
        created_at: now,
    };
    let result_message = signed_protocol_message(
        ctx,
        reviewer,
        &delegation.helper_agent,
        "review-result",
        format!("review result for task {task_id}: {}", decision.as_str()),
        serde_json::json!({
            "review_id": review.id,
            "submission_id": submission.id,
            "delegation_id": delegation_id,
            "decision": decision.as_str(),
            "notes": notes,
        })
        .to_string(),
        Some(submission.message_id),
        Some(task_id.to_string()),
        submission.changed_paths,
    )?;
    store::record_review_result(&ctx.conn, &review, &result_message)?;
    println!("{}", review.id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_agent_list_trims_and_drops_empty() {
        assert_eq!(
            parse_agent_list(Some("claude, gpt-4,, gpt-4-32k ")),
            vec!["claude", "gpt-4", "gpt-4-32k"]
        );
        assert!(parse_agent_list(None).is_empty());
    }

    #[test]
    fn selected_agents_merges_config_and_cli_then_excludes() {
        let mut config = Config::default();
        config.agents.default = vec!["claude".to_string(), "gpt-4".to_string()];
        config.agents.disabled = vec!["blocked".to_string()];
        let selected = selected_agents(&config, Some("gpt-4, gpt-4-32k, blocked"), Some("claude"));
        assert_eq!(selected, vec!["gpt-4", "gpt-4-32k"]);
    }

    #[test]
    fn relaunch_command_uses_project_root_and_agent() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let conn = Connection::open_in_memory().unwrap();
        let config = Config::default();
        let ctx = Context { root, conn, config };
        assert!(relaunch_command_for_agent(&ctx, "alpha").contains("inbox alpha --json"));
    }

    fn migrated_ctx(temp: &tempfile::TempDir) -> Context {
        let root = temp.path().to_path_buf();
        let db_path = root.join(".trelane").join("trelane.db");
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        let conn = crate::db::open(&db_path).unwrap();
        Context {
            root,
            conn,
            config: Config::default(),
        }
    }

    fn assistance_ctx(temp: &tempfile::TempDir) -> Context {
        let ctx = migrated_ctx(temp);
        cmd_add_agent(
            &ctx,
            "owner",
            &["src/**".to_string()],
            &["src/secrets/**".to_string()],
            None,
            None,
        )
        .unwrap();
        cmd_add_agent(&ctx, "helper", &["helper/**".to_string()], &[], None, None).unwrap();
        let now = crypto::now_iso();
        store::insert_task(
            &ctx.conn,
            &Task {
                id: "task_1".to_string(),
                owner_agent: "owner".to_string(),
                domain: "owner".to_string(),
                parent_task: None,
                subject: "add tests".to_string(),
                body: String::new(),
                state: TaskState::Ready,
                priority: "normal".to_string(),
                assist_policy: AssistPolicy::Open,
                desired_parallelism: 1,
                path_scope: vec!["src/**".to_string()],
                acceptance: vec![],
                blocked_by: vec![],
                created_at: now.clone(),
                updated_at: now,
            },
        )
        .unwrap();
        ctx
    }

    fn create_tests_only_offer(ctx: &Context, id: &str) {
        let offer = signed_protocol_message(
            ctx,
            "helper",
            "owner",
            "help-offer",
            "tests offer".to_string(),
            serde_json::json!({
                "delegation_id": id,
                "plan": "add tests",
                "deliverable": "passing tests",
                "allowed_ops": ["write"],
            })
            .to_string(),
            None,
            Some("task_1".to_string()),
            vec!["src/**".to_string()],
        )
        .unwrap();
        store::insert_offer(
            &ctx.conn,
            &Delegation {
                id: id.to_string(),
                task_id: "task_1".to_string(),
                owner_agent: "owner".to_string(),
                helper_agent: "helper".to_string(),
                scope: vec!["src/**".to_string()],
                allowed_ops: vec!["write".to_string()],
                constraints_json: "{}".to_string(),
                base_revision: None,
                offer_message: offer.id.clone(),
                grant_message: String::new(),
                issued_at: crypto::now_iso(),
                expires_at: None,
                status: DelegationStatus::Offered,
            },
            &offer,
        )
        .unwrap();
    }

    fn git_ok(root: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git command failed: {args:?}");
    }

    #[test]
    fn launcher_resolves_known_profile_by_name() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = migrated_ctx(&temp);
        cmd_add_agent(
            &ctx,
            "alpha",
            &["src/**".to_string()],
            &[],
            None,
            Some("opencode"),
        )
        .unwrap();
        let cmd = launcher_command_for_agent(&ctx, "alpha", Path::new("/tmp/p.md"), None).unwrap();
        // The "opencode" profile's default template, not a model-specific one.
        assert!(cmd.starts_with("opencode run \""));
        assert!(!cmd.contains("--model"));
    }

    #[test]
    fn launcher_treats_unknown_launcher_agent_as_a_model_id() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = migrated_ctx(&temp);
        // "openrouter/z-ai/glm-5.2" matches no profile key -- this is exactly
        // what the Biplane UI's model selector stores. Before this fix it
        // silently fell back to the default (claude) launcher.
        cmd_add_agent(
            &ctx,
            "alpha",
            &["src/**".to_string()],
            &[],
            None,
            Some("openrouter/z-ai/glm-5.2"),
        )
        .unwrap();
        let cmd = launcher_command_for_agent(&ctx, "alpha", Path::new("/tmp/p.md"), None).unwrap();
        assert!(cmd.contains("opencode run --model openrouter/z-ai/glm-5.2"));
        assert!(cmd.contains("/tmp/p.md"));
    }

    #[test]
    fn launcher_refuses_when_no_launcher_agent_is_configured() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = migrated_ctx(&temp);
        cmd_add_agent(&ctx, "alpha", &["src/**".to_string()], &[], None, None).unwrap();
        let err = launcher_command_for_agent(&ctx, "alpha", Path::new("/tmp/p.md"), None)
            .expect_err(
                "must refuse rather than silently use the default (possibly paid) launcher",
            );
        assert!(err.is_launcher_not_configured());
        // The refusal must never itself be (or produce) a runnable paid-CLI
        // command -- it's an error, not a fallback command string.
        assert!(err.to_string().contains("launcher-not-configured"));
    }

    #[test]
    fn launcher_override_still_bypasses_the_safety_guard() {
        // An explicit --launcher flag (or --testing-launcher) is a direct,
        // conscious choice made in the moment, distinct from a silent global
        // default -- it must still work even with no launcher_agent stored.
        let temp = tempfile::tempdir().unwrap();
        let ctx = migrated_ctx(&temp);
        cmd_add_agent(&ctx, "alpha", &["src/**".to_string()], &[], None, None).unwrap();
        let cmd = launcher_command_for_agent(
            &ctx,
            "alpha",
            Path::new("/tmp/p.md"),
            Some("trelane --root {root} stub {agent}"),
        )
        .unwrap();
        assert!(cmd.contains("stub alpha"));
    }

    #[test]
    fn command_for_launch_target_wraps_through_tmux_overlay() {
        let target = LaunchTarget {
            agent: "alpha".to_string(),
            adapter: "ghostty".to_string(),
            target: "frontmost".to_string(),
            command: "trelane --root /tmp/demo inbox alpha --json".to_string(),
            tmux_target: Some("trelane-alpha".to_string()),
            updated_at: String::new(),
        };

        let wrapped = command_for_launch_target(&target);
        assert!(wrapped.contains("tmux send-keys -t 'trelane-alpha'"));
        assert!(wrapped.contains("trelane --root /tmp/demo inbox alpha --json"));
    }

    #[test]
    fn shell_double_quote_escapes_quotes_and_backslashes() {
        assert_eq!(
            shell_double_quote("/tmp/a \"quoted\" path"),
            "\"/tmp/a \\\"quoted\\\" path\""
        );
    }

    #[test]
    fn owner_can_narrow_offer_to_tests_only_and_helper_can_claim_only_that_scope() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = assistance_ctx(&temp);
        create_tests_only_offer(&ctx, "del_1");

        let offered_error = authorize_delegated_claim(
            &ctx,
            "helper",
            "del_1",
            None,
            Some("task_1"),
            "src/tests/a.rs",
        )
        .unwrap_err();
        assert!(offered_error.to_string().contains("not active"));

        cmd_help_accept(
            &ctx,
            "del_1",
            "owner",
            &["src/tests/**".to_string()],
            &["write".to_string()],
            3600,
        )
        .unwrap();
        let delegation = store::get_delegation(&ctx.conn, "del_1").unwrap().unwrap();
        assert_eq!(delegation.scope, vec!["src/tests/**".to_string()]);

        let path = ctx.root.join("src/tests/a.rs");
        cmd_claim(
            &ctx,
            "helper",
            &path.to_string_lossy(),
            Some(7200),
            Some("task_1"),
            None,
            Some("del_1"),
        )
        .unwrap();
        let claim = store::get_claim(&ctx.conn, "src/tests/a.rs")
            .unwrap()
            .unwrap();
        assert_eq!(claim.delegation_id.as_deref(), Some("del_1"));
        assert!(claim.expires_human <= delegation.expires_at.unwrap());

        let outside = ctx.root.join("src/lib.rs");
        let error = cmd_claim(
            &ctx,
            "helper",
            &outside.to_string_lossy(),
            None,
            Some("task_1"),
            None,
            Some("del_1"),
        )
        .unwrap_err();
        assert!(error.to_string().contains("does not cover"));

        cmd_help_revoke(&ctx, "del_1", "owner", "scope withdrawn").unwrap();
        assert!(
            store::get_claim(&ctx.conn, "src/tests/a.rs")
                .unwrap()
                .is_none()
        );
        let revoked = authorize_delegated_claim(
            &ctx,
            "helper",
            "del_1",
            None,
            Some("task_1"),
            "src/tests/a.rs",
        )
        .unwrap_err();
        assert!(revoked.to_string().contains("revoked"));
    }

    #[test]
    fn expired_delegation_is_refused_at_command_boundary() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = assistance_ctx(&temp);
        create_tests_only_offer(&ctx, "del_expired");
        cmd_help_accept(
            &ctx,
            "del_expired",
            "owner",
            &["src/tests/**".to_string()],
            &[],
            3600,
        )
        .unwrap();
        ctx.conn
            .execute(
                "UPDATE delegations SET expires_at = '2020-01-01T00:00:00Z' WHERE id = 'del_expired'",
                [],
            )
            .unwrap();
        let path = ctx.root.join("src/tests/a.rs");
        let error = cmd_claim(
            &ctx,
            "helper",
            &path.to_string_lossy(),
            None,
            Some("task_1"),
            None,
            Some("del_expired"),
        )
        .unwrap_err();
        assert!(error.to_string().contains("expired"));
    }

    #[test]
    fn out_of_scope_and_forbidden_submission_paths_fail_closed() {
        let mut task = Task {
            id: "task_1".to_string(),
            owner_agent: "owner".to_string(),
            domain: "owner".to_string(),
            parent_task: None,
            subject: String::new(),
            body: String::new(),
            state: TaskState::Active,
            priority: "normal".to_string(),
            assist_policy: AssistPolicy::Open,
            desired_parallelism: 1,
            path_scope: vec!["src/**".to_string()],
            acceptance: vec![],
            blocked_by: vec![],
            created_at: String::new(),
            updated_at: String::new(),
        };
        let delegation = Delegation {
            id: "del_1".to_string(),
            task_id: task.id.clone(),
            owner_agent: "owner".to_string(),
            helper_agent: "helper".to_string(),
            scope: vec!["src/tests/**".to_string()],
            allowed_ops: vec!["write".to_string()],
            constraints_json: "{}".to_string(),
            base_revision: Some("base".to_string()),
            offer_message: "offer".to_string(),
            grant_message: "grant".to_string(),
            issued_at: String::new(),
            expires_at: None,
            status: DelegationStatus::Active,
        };
        let owner = Domain {
            agent: "owner".to_string(),
            description: String::new(),
            writable: vec!["**".to_string()],
            launcher_agent: None,
            forbidden_write: vec![".trelane/**".to_string(), ".git/**".to_string()],
        };
        assert!(
            validate_submission_paths(&task, &delegation, &owner, &["src/tests/a.rs".to_string()])
                .is_ok()
        );
        assert!(
            validate_submission_paths(&task, &delegation, &owner, &["src/lib.rs".to_string()])
                .unwrap_err()
                .to_string()
                .contains("outside delegation scope")
        );
        task.path_scope = vec!["**".to_string()];
        let mut broad = delegation;
        broad.scope = vec!["**".to_string()];
        assert!(
            validate_submission_paths(&task, &broad, &owner, &[".git/config".to_string()])
                .unwrap_err()
                .to_string()
                .contains("hard-forbidden")
        );
    }

    #[test]
    fn git_submission_and_owner_review_run_end_to_end() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        git_ok(root, &["init", "-q"]);
        git_ok(root, &["config", "user.email", "tests@example.invalid"]);
        git_ok(root, &["config", "user.name", "Trelane Tests"]);
        std::fs::create_dir_all(root.join("src/tests")).unwrap();
        std::fs::write(root.join("src/tests/a.rs"), "const A: u8 = 1;\n").unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn baseline() {}\n").unwrap();
        git_ok(root, &["add", "src"]);
        git_ok(root, &["commit", "-q", "-m", "base"]);

        let ctx = assistance_ctx(&temp);
        cmd_help_offer(
            &ctx,
            "helper",
            "owner",
            "task_1",
            &["src/tests/**".to_string()],
            "add coverage",
            "tests",
            &["write".to_string()],
        )
        .unwrap();
        let offer = store::list_open_offers_for_owner(&ctx.conn, "owner")
            .unwrap()
            .pop()
            .unwrap();
        cmd_help_accept(&ctx, &offer.id, "owner", &[], &[], 3600).unwrap();

        std::fs::write(root.join("src/lib.rs"), "pub fn outside_scope() {}\n").unwrap();
        git_ok(root, &["add", "src/lib.rs"]);
        git_ok(root, &["commit", "-q", "-m", "outside"]);
        let outside_commit = git_head(root).unwrap();
        let error = cmd_work_submit(
            &ctx,
            "task_1",
            "helper",
            &offer.id,
            &outside_commit,
            "bad diff",
            "not run",
        )
        .unwrap_err();
        assert!(error.to_string().contains("outside delegation scope"));

        // Restore the out-of-scope file to its base contents and create a
        // final tree whose base..commit diff contains only the delegated test.
        std::fs::write(root.join("src/lib.rs"), "pub fn baseline() {}\n").unwrap();
        std::fs::write(root.join("src/tests/a.rs"), "const A: u8 = 2;\n").unwrap();
        git_ok(root, &["add", "src"]);
        git_ok(root, &["commit", "-q", "-m", "scoped tests"]);
        let scoped_commit = git_head(root).unwrap();
        cmd_work_submit(
            &ctx,
            "task_1",
            "helper",
            &offer.id,
            &scoped_commit,
            "added coverage",
            "cargo test",
        )
        .unwrap();
        cmd_work_review(
            &ctx, "task_1", "owner", &offer.id, true, false, false, "approved",
        )
        .unwrap();
        assert_eq!(
            store::get_task(&ctx.conn, "task_1").unwrap().unwrap().state,
            TaskState::Done
        );
        assert_eq!(
            store::get_delegation(&ctx.conn, &offer.id)
                .unwrap()
                .unwrap()
                .status,
            DelegationStatus::Accepted
        );
    }

    #[test]
    fn stale_base_submission_invalidates_delegation() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        git_ok(root, &["init", "-q"]);
        git_ok(root, &["config", "user.email", "tests@example.invalid"]);
        git_ok(root, &["config", "user.name", "Trelane Tests"]);
        std::fs::create_dir_all(root.join("src/tests")).unwrap();
        std::fs::write(root.join("src/tests/a.rs"), "const A: u8 = 1;\n").unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn baseline() {}\n").unwrap();
        git_ok(root, &["add", "src"]);
        git_ok(root, &["commit", "-q", "-m", "base"]);
        let owner_branch = git_output(root, &["symbolic-ref", "--short", "HEAD"]).unwrap();

        let ctx = assistance_ctx(&temp);
        cmd_help_offer(
            &ctx,
            "helper",
            "owner",
            "task_1",
            &["src/tests/**".to_string()],
            "add coverage",
            "tests",
            &["write".to_string()],
        )
        .unwrap();
        let offer = store::list_open_offers_for_owner(&ctx.conn, "owner")
            .unwrap()
            .pop()
            .unwrap();
        cmd_help_accept(&ctx, &offer.id, "owner", &[], &[], 3600).unwrap();

        git_ok(root, &["checkout", "-q", "-b", "helper-work"]);
        std::fs::write(root.join("src/tests/a.rs"), "const A: u8 = 2;\n").unwrap();
        git_ok(root, &["add", "src/tests/a.rs"]);
        git_ok(root, &["commit", "-q", "-m", "helper tests"]);
        let helper_commit = git_head(root).unwrap();

        git_ok(root, &["checkout", "-q", &owner_branch]);
        std::fs::write(root.join("src/lib.rs"), "pub fn owner_advanced() {}\n").unwrap();
        git_ok(root, &["add", "src/lib.rs"]);
        git_ok(root, &["commit", "-q", "-m", "owner advanced"]);

        let error = cmd_work_submit(
            &ctx,
            "task_1",
            "helper",
            &offer.id,
            &helper_commit,
            "tests",
            "cargo test",
        )
        .unwrap_err();
        assert!(error.to_string().contains("stale"));
        assert_eq!(
            store::get_delegation(&ctx.conn, &offer.id)
                .unwrap()
                .unwrap()
                .status,
            DelegationStatus::Revoked
        );
    }
}

/// F1: Resolve parked tasks whose `waiting_on` is a disabled/removed agent.
/// Wakes the waiting agent immediately with an abandonment reason instead
/// of waiting for the next squire tick.
pub fn resolve_dangling_parks_for(ctx: &Context, disabled_agent: &str) -> Result<()> {
    let all_parked = store::list_parked_tasks(&ctx.conn)?;
    let dangling: Vec<_> = all_parked
        .iter()
        .filter(|e| e.waiting_on == disabled_agent)
        .collect();

    if dangling.is_empty() {
        return Ok(());
    }

    let secret = ctx.secret()?;

    for entry in &dangling {
        if prompt::park_satisfied(&ctx.conn, entry)? {
            continue;
        }

        // Delete the abandoned park so it stops blocking.
        let _ = store::delete_parked_task(&ctx.conn, &entry.task);

        // Send an abandonment info message to the waiting agent.
        let mut msg = Message::new(
            crypto::new_id("msg"),
            "system".to_string(),
            entry.agent.clone(),
            "system".to_string(),
            "high".to_string(),
            format!("park abandoned: agent '{}' was disabled", disabled_agent),
            format!(
                "Your parked task '{}' was waiting on '{}' which has been disabled. \
                The park has been cleared. Proceed with a documented assumption or \
                escalate to the user.",
                entry.task, disabled_agent
            ),
            None,
            None,
            vec![],
            crypto::now_iso(),
        );
        crypto::sign(&secret, &mut msg);
        store::insert_message(&ctx.conn, &msg)?;

        eprintln!(
            "{} proactive abandonment: woke {} (was waiting on disabled {})",
            crypto::now_iso(),
            entry.agent,
            disabled_agent
        );
    }

    Ok(())
}
