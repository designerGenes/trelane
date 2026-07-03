use crate::error::{Result, TrelaneError};
use crate::models::{Domain, TRELANE_DIR};
use globset::Glob;
use std::path::{Path, PathBuf};

pub struct CompiledDomain {
    writable: Vec<globset::GlobMatcher>,
    forbidden: Vec<globset::GlobMatcher>,
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
        forbidden_write: vec![format!("{TRELANE_DIR}/**"), ".git/**".to_string()],
    }
}

pub fn norm_rel(root: &Path, path: &str) -> Result<String> {
    let p = Path::new(path);
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
            forbidden_write: forbidden.iter().map(|s| s.to_string()).collect(),
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
    fn default_forbids_trelane_internals() {
        let dom = default_domain("test");
        let compiled = CompiledDomain::from_domain(&dom).unwrap();
        assert!(!compiled.is_writable(".trelane/secret"));
        assert!(!compiled.is_writable(".trelane/trelane.db"));
        assert!(!compiled.is_writable(".git/config"));
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
