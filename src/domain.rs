use crate::error::{Result, TrelaneError};
use crate::models::{default_granularity_tier, Domain, TRELANE_DIR};
use globset::Glob;
use std::path::{Path, PathBuf};

pub struct CompiledDomain {
    writable: Vec<globset::GlobMatcher>,
    forbidden: Vec<globset::GlobMatcher>,
}

fn has_glob_meta(value: &str) -> bool {
    value
        .bytes()
        .any(|b| matches!(b, b'*' | b'?' | b'[' | b'{' | b'!'))
}

fn literal_prefix(pattern: &str) -> &str {
    let end = pattern
        .char_indices()
        .find(|(_, c)| matches!(c, '*' | '?' | '[' | '{' | '!'))
        .map(|(i, _)| i)
        .unwrap_or(pattern.len());
    pattern[..end].trim_end_matches('/')
}

/// True when `candidate` can be conservatively proven to be contained by
/// `parent`. Exact entries, concrete paths covered by a glob, and narrower
/// descendants of a `/**` scope are provable. Everything else fails closed.
pub fn scope_entry_is_subset(candidate: &str, parent: &str) -> Result<bool> {
    if candidate == parent {
        return Ok(true);
    }
    if parent == "**" {
        return Ok(true);
    }
    if !has_glob_meta(candidate) {
        return Ok(Glob::new(parent)?.compile_matcher().is_match(candidate));
    }
    if let Some(prefix) = parent.strip_suffix("/**") {
        let prefix = prefix.trim_end_matches('/');
        if prefix.is_empty() {
            return Ok(true);
        }
        let candidate_prefix = literal_prefix(candidate);
        return Ok((candidate_prefix == prefix
            && candidate
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with('/')))
            || candidate_prefix
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with('/')));
    }
    Ok(false)
}

/// True when two scope expressions might select at least one common path.
/// This is intentionally conservative: uncertainty is treated as overlap so
/// forbidden scopes always take precedence.
pub fn scope_entries_may_overlap(a: &str, b: &str) -> Result<bool> {
    let a_glob = has_glob_meta(a);
    let b_glob = has_glob_meta(b);
    if !a_glob {
        return Ok(Glob::new(b)?.compile_matcher().is_match(a));
    }
    if !b_glob {
        return Ok(Glob::new(a)?.compile_matcher().is_match(b));
    }
    let ap = literal_prefix(a);
    let bp = literal_prefix(b);
    if ap.is_empty() || bp.is_empty() {
        return Ok(true);
    }
    // A glob metacharacter may complete the remainder of the same path
    // segment (`.[g]it/**` vs `.git/**`), so raw prefix overlap is the safe
    // answer. False positives reject a scope; false negatives could delegate
    // a forbidden path.
    Ok(ap.starts_with(bp) || bp.starts_with(ap))
}

pub fn scope_covers_path(scope: &[String], rel: &str) -> Result<bool> {
    for entry in scope {
        if Glob::new(entry)?.compile_matcher().is_match(rel) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Hard protocol internals are never writable, even if an old domain row did
/// not persist the default forbidden globs.
pub fn is_hard_forbidden(rel: &str) -> bool {
    rel == TRELANE_DIR
        || rel.starts_with(&format!("{TRELANE_DIR}/"))
        || rel == ".git"
        || rel.starts_with(".git/")
}

/// Validate that an entire proposed scope is owned by `dom` and cannot
/// intersect either a configured forbidden scope or Trelane/Git internals.
pub fn domain_allows_scope(dom: &Domain, candidate: &str) -> Result<bool> {
    if is_hard_forbidden(candidate)
        || candidate.starts_with(&format!("{TRELANE_DIR}/"))
        || candidate.starts_with(".git/")
    {
        return Ok(false);
    }
    if !dom
        .writable
        .iter()
        .any(|parent| scope_entry_is_subset(candidate, parent).unwrap_or(false))
    {
        return Ok(false);
    }
    for forbidden in dom
        .forbidden_write
        .iter()
        .chain([format!("{TRELANE_DIR}/**"), ".git/**".to_string()].iter())
    {
        if scope_entries_may_overlap(candidate, forbidden)? {
            return Ok(false);
        }
    }
    Ok(true)
}

impl CompiledDomain {
    pub fn from_domain(dom: &Domain) -> Result<Self> {
        let writable = dom
            .writable
            .iter()
            .map(|g| Ok(Glob::new(g)?.compile_matcher()))
            .collect::<Result<Vec<_>>>()?;
        let forbidden = dom
            .forbidden_write
            .iter()
            .map(|g| Ok(Glob::new(g)?.compile_matcher()))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            writable,
            forbidden,
        })
    }

    pub fn is_writable(&self, rel: &str) -> bool {
        if self.forbidden.iter().any(|m| m.is_match(rel)) {
            return false;
        }
        self.writable.iter().any(|m| m.is_match(rel))
    }
}

pub fn default_domain(agent: &str) -> Domain {
    Domain {
        agent: agent.to_string(),
        description: String::new(),
        writable: vec![],
        launcher_agent: None,
        forbidden_write: vec![format!("{TRELANE_DIR}/**"), ".git/**".to_string()],
        // v13 (Slice 5): a freshly-created domain starts at the coarsest tier
        // with no lineage and no split metadata; refinement fills these in.
        granularity_tier: default_granularity_tier(),
        parent_domain: None,
        created_in_pass: 0,
        owner_at_split_time: None,
        tier_set_at: None,
    }
}

pub fn norm_rel(root: &Path, path: &str) -> Result<String> {
    let p = Path::new(path);
    if p.components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(TrelaneError::Msg(format!(
            "path contains parent traversal: {path}"
        )));
    }
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()?.join(p)
    };
    let rel = abs
        .strip_prefix(root)
        .map_err(|_| TrelaneError::Msg(format!("path escapes project root: {path}")))?;
    let s = rel
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/");
    if s.starts_with("..") {
        return Err(TrelaneError::Msg(format!(
            "path escapes project root: {path}"
        )));
    }
    Ok(s)
}

pub fn find_root(explicit: Option<&Path>) -> Result<PathBuf> {
    let start = match explicit {
        Some(p) => p.to_path_buf(),
        None => match std::env::var_os("TRELANE_ROOT") {
            Some(s) => PathBuf::from(s),
            None => std::env::current_dir()?,
        },
    };
    let abs = if start.is_absolute() {
        start
    } else {
        std::env::current_dir()?.join(start)
    };
    let mut p = abs.as_path();
    loop {
        if p.join(TRELANE_DIR).is_dir() {
            return Ok(p.to_path_buf());
        }
        match p.parent() {
            Some(parent) => p = parent,
            None => {
                return Err(TrelaneError::Msg(
                    "no .trelane directory found here or above; run 'trelane init' first".into(),
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_domain(writable: &[&str], forbidden: &[&str]) -> Domain {
        Domain {
            agent: "test".to_string(),
            description: String::new(),
            writable: writable.iter().map(|s| s.to_string()).collect(),
            launcher_agent: None,
            forbidden_write: forbidden.iter().map(|s| s.to_string()).collect(),
            granularity_tier: default_granularity_tier(),
            parent_domain: None,
            created_in_pass: 0,
            owner_at_split_time: None,
            tier_set_at: None,
        }
    }

    #[test]
    fn double_star_matches_nested() {
        let dom = make_domain(&["src/ui/**"], &[]);
        let compiled = CompiledDomain::from_domain(&dom).unwrap();
        assert!(compiled.is_writable("src/ui/app.ts"));
        assert!(compiled.is_writable("src/ui/components/Button.tsx"));
        assert!(!compiled.is_writable("src/api/routes.py"));
    }

    #[test]
    fn forbidden_overrides_writable() {
        let dom = make_domain(&["src/**"], &["src/secrets/**"]);
        let compiled = CompiledDomain::from_domain(&dom).unwrap();
        assert!(compiled.is_writable("src/ui/app.ts"));
        assert!(!compiled.is_writable("src/secrets/key.pem"));
    }

    #[test]
    fn proves_tests_only_narrowing_but_fails_closed_on_ambiguous_globs() {
        assert!(scope_entry_is_subset("src/tests/**", "src/**").unwrap());
        assert!(scope_entry_is_subset("src/tests/unit.rs", "src/tests/**").unwrap());
        assert!(!scope_entry_is_subset("src/lib.rs", "src/tests/**").unwrap());
        assert!(!scope_entry_is_subset("src/*/unit.rs", "src/?ests/**").unwrap());
    }

    #[test]
    fn hard_forbidden_precedence_applies_to_delegable_scopes() {
        let dom = make_domain(&["**"], &[]);
        assert!(!domain_allows_scope(&dom, ".trelane/**").unwrap());
        assert!(!domain_allows_scope(&dom, ".git/config").unwrap());
        assert!(domain_allows_scope(&dom, "src/tests/**").unwrap());
    }

    #[test]
    fn default_forbids_trelane_internals() {
        let dom = default_domain("test");
        let compiled = CompiledDomain::from_domain(&dom).unwrap();
        assert!(!compiled.is_writable(".trelane/secret"));
        assert!(!compiled.is_writable(".trelane/trelane.db"));
        assert!(!compiled.is_writable(".git/config"));
    }

    #[test]
    fn parent_traversal_cannot_bypass_hard_forbidden_paths() {
        let root = Path::new("/tmp/project");
        assert!(norm_rel(root, "/tmp/project/src/../.git/config").is_err());
        assert!(norm_rel(root, "src/../../.trelane/secret").is_err());
    }

    #[test]
    fn empty_domain_writes_nothing() {
        let dom = make_domain(&[], &[]);
        let compiled = CompiledDomain::from_domain(&dom).unwrap();
        assert!(!compiled.is_writable("anything"));
    }

    #[test]
    fn valid_agent_names() {
        assert!(is_valid_name("alpha"));
        assert!(is_valid_name("backend-2"));
        assert!(is_valid_name("my_agent"));
        assert!(!is_valid_name(""));
        assert!(!is_valid_name("UPPER"));
        assert!(!is_valid_name("has space"));
        assert!(!is_valid_name(&"x".repeat(33)));
    }

    fn is_valid_name(name: &str) -> bool {
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
}
