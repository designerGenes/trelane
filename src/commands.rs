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
use std::process::{Command, Stdio};

// ----------------------------------------------------------------- helpers

pub fn is_running(conn: &Connection, agent: &str) -> Result<bool> {
    match store::get_running_lock(conn, agent)? {
        None => Ok(false),
        Some(lock) => {
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

fn grant_covers(
    conn: &Connection,
    agent: &str,
    grant_msg_id: &str,
    rel: &str,
    secret: &[u8],
) -> Result<bool> {
    if let Some(msg) = store::get_message(conn, grant_msg_id)?
        && msg.to == agent
        && msg.msg_type == "claim-grant"
        && crypto::verify(secret, &msg)
        && msg.paths.iter().any(|p| p == rel)
    {
        let owners = owners_of(conn, rel, None)?;
        return Ok(owners.contains(&msg.from));
    }
    Ok(false)
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
        "secret\nagents/*/.prompt.md\nagents/*/logs/\npump.log\n*.db-wal\n*.db-shm\n",
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

fn domain_launch_enabled(ctx: &Context, domain_agent: &str) -> Result<bool> {
    let domain = store::get_domain(&ctx.conn, domain_agent)?
        .ok_or_else(|| TrelaneError::msg(format!("unknown agent '{domain_agent}'")))?;
    match domain.launcher_agent.as_deref() {
        Some(session_agent) => is_agent_enabled(ctx, session_agent),
        None => Ok(true),
    }
}

fn relaunch_command_for_agent(ctx: &Context, agent: &str) -> String {
    format!(
        "trelane --root {} inbox {} --json",
        ctx.root.display(),
        agent
    )
}

fn applescript_escape(input: &str) -> String {
    input.replace('\\', "\\\\").replace('"', "\\\"")
}

fn launch_via_adapter(adapter: &str, target: &str, command: &str) -> Result<()> {
    let status = match adapter {
        "tmux" => Command::new("tmux")
            .args(["send-keys", "-t", target, command, "Enter"])
            .status()?,
        "kitty" => Command::new("kitty")
            .args(["@", "send-text", "--match", target, &format!("{command}\n")])
            .status()?,
        "wezterm" => Command::new("wezterm")
            .args(["cli", "send-text", "--pane-id", target, command])
            .status()?,
        "ghostty" => {
            let script = if target == "frontmost" {
                format!(
                    "tell application \"Ghostty\" to activate\n\
                     tell application \"System Events\"\n\
                     tell process \"Ghostty\"\n\
                     keystroke \"{}\"\n\
                     key code 36\n\
                     end tell\n\
                     end tell",
                    applescript_escape(command)
                )
            } else {
                format!(
                    "tell application \"Ghostty\" to activate\n\
                     tell application \"System Events\"\n\
                     tell process \"Ghostty\"\n\
                     set frontmost to true\n\
                     try\n\
                     click (first window whose name contains \"{}\")\n\
                     end try\n\
                     keystroke \"{}\"\n\
                     key code 36\n\
                     end tell\n\
                     end tell",
                    applescript_escape(target),
                    applescript_escape(command)
                )
            };
            Command::new("osascript").args(["-e", &script]).status()?
        }
        "iterm2" => Command::new("osascript")
            .args([
                "-e",
                &format!(
                    "tell application \"iTerm2\" to tell current session of current window to write text \"{}\"",
                    applescript_escape(command)
                ),
            ])
            .status()?,
        "terminal.app" => Command::new("osascript")
            .args([
                "-e",
                &format!(
                    "tell application \"Terminal\" to do script \"{}\" in selected tab of front window",
                    applescript_escape(command)
                ),
            ])
            .status()?,
        other => return Err(TrelaneError::msg(format!("unsupported adapter '{other}'"))),
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

    let forbidden = vec![format!("{TRELANE_DIR}/**"), ".git/**".to_string()];
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
    desc: Option<&str>,
) -> Result<()> {
    if !store::agent_exists(&ctx.conn, agent)? {
        return Err(TrelaneError::msg(format!("unknown agent '{agent}'")));
    }

    let existing = store::get_domain(&ctx.conn, agent)?
        .ok_or_else(|| TrelaneError::msg(format!("unknown agent '{agent}'")))?;

    let forbidden = vec![format!("{TRELANE_DIR}/**"), ".git/**".to_string()];
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
) -> Result<()> {
    if !store::agent_exists(&ctx.conn, agent)? {
        return Err(TrelaneError::msg(format!("unknown agent '{agent}'")));
    }
    let rel = domain::norm_rel(&ctx.root, path)?;
    if rel.starts_with(".trelane/") || rel == ".trelane" {
        return Err(TrelaneError::msg("never claim .trelane internals"));
    }

    let ttl = ttl.unwrap_or(ctx.config.claims.default_ttl_s);
    let now_ts = chrono::Utc::now().timestamp() as f64;
    let expires_at = now_ts + ttl as f64;
    let expires_human = chrono::Utc::now()
        .checked_add_signed(chrono::Duration::seconds(ttl as i64))
        .map(|t| t.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_default();

    // Check existing lease
    if let Some(lease) = store::get_claim(&ctx.conn, &rel)? {
        if lease.expires_at >= now_ts {
            if lease.holder == agent {
                store::update_claim_expiry(&ctx.conn, &rel, expires_at, &expires_human)?;
                println!("renewed lease on {rel} (ttl {ttl}s)");
                return Ok(());
            }
            eprintln!(
                "DENIED: {rel} is leased by {} until {}.",
                lease.holder, lease.expires_human
            );
            eprintln!(
                "hint: send a claim-request to {}, park on the reply, and exit cleanly.",
                lease.holder
            );
            std::process::exit(2);
        } else {
            // Expired — reap
            store::delete_claim(&ctx.conn, &rel)?;
        }
    }

    let dom = store::get_domain(&ctx.conn, agent)?
        .ok_or_else(|| TrelaneError::msg(format!("agent '{agent}' not found")))?;
    let compiled = CompiledDomain::from_domain(&dom)?;
    let mine = compiled.is_writable(&rel);
    let others = owners_of(&ctx.conn, &rel, Some(agent))?;

    let secret = ctx.secret()?;

    if !mine && !others.is_empty() {
        let has_grant = grant
            .map(|g| grant_covers(&ctx.conn, agent, g, &rel, &secret))
            .transpose()?
            .unwrap_or(false);
        if !has_grant {
            eprintln!(
                "DENIED: {rel} is in the domain of {} and not yours.",
                others.join(", ")
            );
            eprintln!(
                "hint: send a claim-request to the owner; claim again with --grant <claim-grant-msg-id> once granted."
            );
            std::process::exit(3);
        }
    }

    let new_lease = Lease {
        path: rel.clone(),
        holder: agent.to_string(),
        task: task.map(|s| s.to_string()),
        grant: grant.map(|s| s.to_string()),
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
        Ok(false) => {
            eprintln!(
                "DENIED: lost race for {rel}; re-check with 'trelane claim' later or park on it."
            );
            std::process::exit(2);
        }
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
        wait_type,
        wait_re,
        wait_path,
        waiting_on: waiting_on.to_string(),
        resume_hint: resume_hint.to_string(),
        created_at: crypto::now_iso(),
    };
    store::insert_parked_task(&ctx.conn, &entry)?;
    println!("{task_id}");
    Ok(())
}

// ---------------------------------------------------------------- unpark

pub fn cmd_unpark(ctx: &Context, task: &str) -> Result<()> {
    store::delete_parked_task(&ctx.conn, task)?;
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

    let launch_targets = store::list_launch_targets(&ctx.conn)?;
    if !launch_targets.is_empty() {
        println!("launch targets:");
        for target in &launch_targets {
            println!(
                "  {:<16} adapter={} target={} command={}",
                target.agent, target.adapter, target.target, target.command
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

    let (_, cycle) = crate::pump::wait_graph(&ctx.conn)?;
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

    if let Some(dirty) = git_dirty(&ctx.root) {
        store::save_audit_baseline(&ctx.conn, agent, &dirty)?;
    }

    if launcher_override.is_none()
        && let Some(target) = store::get_launch_target(&ctx.conn, agent)?
    {
        launch_via_adapter(&target.adapter, &target.target, &target.command)?;
        let inserted =
            store::insert_running_lock(&ctx.conn, agent, -1, &crypto::now_iso(), reason)?;
        if !inserted {
            eprintln!("warning: {agent} was already launched by another pump");
        }
        println!(
            "relaunched {agent} via {} target={} reason={reason}",
            target.adapter, target.target
        );
        return Ok(());
    }

    let template = launcher_override.unwrap_or(&ctx.config.launcher.template);
    let cmd = template
        .replace("{prompt_file}", &prompt_file.display().to_string())
        .replace("{agent}", agent)
        .replace("{root}", &ctx.root.display().to_string());

    let log_dir = ctx.trelane_dir().join("agents").join(agent).join("logs");
    std::fs::create_dir_all(&log_dir)?;
    let log_name = format!("run-{}.log", crypto::new_id("r"));
    let log_path = log_dir.join(&log_name);
    let log_file = std::fs::File::create(&log_path)?;

    use std::os::unix::process::CommandExt;
    let mut command = std::process::Command::new("sh");
    command
        .arg("-c")
        .arg(&cmd)
        .current_dir(&ctx.root)
        .stdout(Stdio::from(log_file.try_clone()?))
        .stderr(Stdio::from(log_file))
        .process_group(0);

    let child = command.spawn()?;
    let pid = child.id() as i32;
    std::mem::forget(child);

    let inserted = store::insert_running_lock(&ctx.conn, agent, pid, &crypto::now_iso(), reason)?;
    if !inserted {
        eprintln!("warning: {agent} was already launched by another pump");
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
) -> Result<()> {
    if !store::agent_exists(&ctx.conn, agent)? {
        return Err(TrelaneError::msg(format!("unknown agent '{agent}'")));
    }
    if !domain_launch_enabled(ctx, agent)? {
        return Err(TrelaneError::msg(format!(
            "agent '{agent}' is mapped to a disabled session agent/model"
        )));
    }
    let command = command
        .map(str::to_string)
        .unwrap_or_else(|| relaunch_command_for_agent(ctx, agent));
    store::upsert_launch_target(
        &ctx.conn,
        agent,
        adapter,
        target,
        &command,
        &crypto::now_iso(),
    )?;
    println!("stored launch target for {agent}: {adapter} {target}");
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
        .unwrap_or_else(|| relaunch_command_for_agent(ctx, agent));

    launch_via_adapter(&adapter, &target, &command)?;
    println!("relaunched {agent} via {adapter} target={target}");
    Ok(())
}

// ------------------------------------------------------------------ done

pub fn cmd_done(ctx: &Context, agent: &str) -> Result<()> {
    store::delete_running_lock(&ctx.conn, agent)?;
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
}
