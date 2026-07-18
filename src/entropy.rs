//! Deadlock-likelihood ("entropy") scoring for a Biplane analysis.
//!
//! This is a *static* estimate: given a proposed domain split, how structurally
//! prone is this project to the coordination stalls Trelane exists to prevent?
//! It is computed once during Biplane analysis and stored in the report, so the
//! diagnostics UI can display it without recomputing. It is deliberately NOT a
//! live runtime signal — a live "are we deadlocked right now" answer already
//! comes from `squire::wait_graph`. The two are complementary: this predicts
//! risk from project shape; the wait-graph observes actual state.
//!
//! Everything here is a pure function of structural inputs, so it is fully
//! unit-tested and has zero I/O. Kept in its own module so it can be scored,
//! reasoned about, and merged independently of both the analyzer and the UI.

use serde::{Deserialize, Serialize};

/// The structural inputs the score is derived from. Each is something Biplane
/// already knows at analysis time — no new analysis pass is required, just a
/// reading of the domain graph it already produced.
#[derive(Debug, Clone, Copy)]
pub struct EntropyInputs {
    /// Number of domains in the proposed split.
    pub domain_count: usize,
    /// Number of `depends_on` edges across all domains. More ordering
    /// constraints = more ways for work to block on other work.
    pub dependency_edges: usize,
    /// Count of domain *pairs* whose writable globs overlap. Overlap is the
    /// raw material of cross-domain contention: two agents that can both write
    /// the same path will contend for it.
    pub writable_overlaps: usize,
    /// Length of the longest dependency chain (0 if no dependencies). A deep
    /// chain means late domains wait through many predecessors.
    pub longest_chain: usize,
    /// Whether the dependency graph as proposed already contains a cycle. A
    /// static cycle is the strongest possible signal: it guarantees a runtime
    /// deadlock unless broken.
    pub has_static_cycle: bool,
}

/// A computed entropy score plus the human-readable reasons behind it, so the
/// UI can show not just a number but *why* — which is what makes it actionable
/// rather than mysterious.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EntropyScore {
    /// 0–100. Higher = more structurally deadlock-prone. Banded by `level()`.
    pub score: u8,
    /// Short factors that pushed the score up, most significant first.
    pub factors: Vec<String>,
}

/// Qualitative band for a score, for color/label in the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntropyLevel {
    Low,
    Moderate,
    High,
    Critical,
}

impl EntropyLevel {
    pub fn label(&self) -> &'static str {
        match self {
            EntropyLevel::Low => "LOW",
            EntropyLevel::Moderate => "MODERATE",
            EntropyLevel::High => "HIGH",
            EntropyLevel::Critical => "CRITICAL",
        }
    }
}

impl EntropyScore {
    pub fn level(&self) -> EntropyLevel {
        match self.score {
            0..=24 => EntropyLevel::Low,
            25..=49 => EntropyLevel::Moderate,
            50..=79 => EntropyLevel::High,
            _ => EntropyLevel::Critical,
        }
    }
}

/// Compute the entropy score from structural inputs. Pure and total.
///
/// The weighting rationale, made explicit so it can be argued with rather than
/// trusted blindly:
/// - A pre-existing static cycle dominates everything (it's a guaranteed
///   deadlock), so it alone floors the score into CRITICAL.
/// - Writable overlaps are the next-worst signal: they're the direct cause of
///   the contention DI and claims exist to arbitrate.
/// - Dependency density (edges relative to domains) and chain depth contribute
///   moderately: they raise the odds of waiting, without guaranteeing a stall.
/// - A single domain, or fully independent domains, score near zero: with no
///   cross-domain edges there is nothing to deadlock on.
pub fn compute(inputs: EntropyInputs) -> EntropyScore {
    let mut score: u32 = 0;
    let mut factors: Vec<String> = Vec::new();

    if inputs.has_static_cycle {
        // Guaranteed runtime deadlock unless broken. Floor at CRITICAL.
        factors.push("dependency graph already contains a cycle".to_string());
        return EntropyScore {
            score: 90u8.max(overlap_component(&inputs, &mut factors).min(100) as u8),
            factors,
        };
    }

    // Writable overlaps: the strongest non-cycle signal.
    let overlap = overlap_component(&inputs, &mut factors);
    score += overlap;

    // Dependency density: edges per domain. 0 domains or 1 domain => no density.
    if inputs.domain_count > 1 {
        let density = (inputs.dependency_edges * 100) / inputs.domain_count;
        // Cap the density contribution so a heavily-sequenced-but-acyclic
        // project doesn't alone reach CRITICAL.
        let density_pts = (density / 4).min(25) as u32;
        if density_pts > 0 {
            factors.push(format!(
                "{} dependency edge(s) across {} domains",
                inputs.dependency_edges, inputs.domain_count
            ));
            score += density_pts;
        }
    }

    // Chain depth: a long critical path means deep waiting.
    if inputs.longest_chain >= 3 {
        let chain_pts = ((inputs.longest_chain - 2) * 5).min(20) as u32;
        factors.push(format!(
            "longest dependency chain is {} domains deep",
            inputs.longest_chain
        ));
        score += chain_pts;
    }

    // A single catch-all domain has no cross-domain coordination surface at
    // all — note that as a reassuring factor rather than leaving factors empty.
    if inputs.domain_count <= 1 {
        factors.push("single domain: no cross-domain coordination surface".to_string());
    } else if factors.is_empty() {
        factors.push("domains are independent (no dependencies or overlaps)".to_string());
    }

    EntropyScore {
        score: score.min(100) as u8,
        factors,
    }
}

/// The writable-overlap contribution, shared between the cycle and non-cycle
/// paths. Pushes a factor string when non-zero.
fn overlap_component(inputs: &EntropyInputs, factors: &mut Vec<String>) -> u32 {
    if inputs.writable_overlaps == 0 {
        return 0;
    }
    let pts = (inputs.writable_overlaps * 15).min(45) as u32;
    factors.push(format!(
        "{} domain pair(s) with overlapping writable globs",
        inputs.writable_overlaps
    ));
    pts
}

/// A minimal, dependency-free view of a domain for input extraction, so this
/// module doesn't depend on the analyzer's richer `DomainSpec` type and can be
/// tested in isolation. Callers map their own domain type into this.
#[derive(Debug, Clone)]
pub struct DomainView {
    pub name: String,
    pub writable: Vec<String>,
    pub depends_on: Vec<String>,
}

/// Derive [`EntropyInputs`] from a set of domains plus whether the live
/// wait-graph already shows a cycle. Pure: the overlap count, edge count, and
/// longest-chain computation are all done here and unit-tested, rather than
/// hand-inlined at the call site where they couldn't be.
///
/// `has_static_cycle` is passed in (not computed here) because the authoritative
/// cycle detector already lives in `squire::wait_graph` — re-implementing it
/// here would risk the two disagreeing. This function counts *structural*
/// risk factors; it defers the one definitive signal to the existing detector.
pub fn inputs_from_domains(domains: &[DomainView], has_static_cycle: bool) -> EntropyInputs {
    let domain_count = domains.len();

    let dependency_edges: usize = domains.iter().map(|d| d.depends_on.len()).sum();

    // Count unordered domain pairs whose writable globs overlap. Overlap here
    // is prefix-based: two globs overlap if either is a path-prefix of the
    // other (e.g. "src/**" vs "src/api/**"), which is the cheap, deterministic
    // approximation the analyzer already reasons about. Exact glob-intersection
    // is deliberately not attempted — a prefix check is enough to flag the
    // contention risk without a glob engine.
    let mut writable_overlaps = 0usize;
    for i in 0..domain_count {
        for j in (i + 1)..domain_count {
            if globs_overlap(&domains[i].writable, &domains[j].writable) {
                writable_overlaps += 1;
            }
        }
    }

    let longest_chain = longest_dependency_chain(domains);

    EntropyInputs {
        domain_count,
        dependency_edges,
        writable_overlaps,
        longest_chain,
        has_static_cycle,
    }
}

/// True if any glob in `a` and any glob in `b` share a path-prefix relationship.
fn globs_overlap(a: &[String], b: &[String]) -> bool {
    for ga in a {
        let pa = glob_prefix(ga);
        for gb in b {
            let pb = glob_prefix(gb);
            if pa == pb || pa.starts_with(&pb) || pb.starts_with(&pa) {
                return true;
            }
        }
    }
    false
}

/// The literal path portion of a glob, up to the first wildcard segment.
fn glob_prefix(glob: &str) -> String {
    glob.split('/')
        .take_while(|seg| !seg.contains('*'))
        .collect::<Vec<_>>()
        .join("/")
}

/// Longest chain length following `depends_on` edges. Guards against cycles by
/// bounding recursion at the domain count (a cycle can't legitimately make a
/// chain longer than every domain once).
fn longest_dependency_chain(domains: &[DomainView]) -> usize {
    use std::collections::HashMap;
    let by_name: HashMap<&str, &DomainView> =
        domains.iter().map(|d| (d.name.as_str(), d)).collect();

    fn depth(
        name: &str,
        by_name: &std::collections::HashMap<&str, &DomainView>,
        budget: usize,
    ) -> usize {
        if budget == 0 {
            return 0; // cycle guard: stop descending
        }
        match by_name.get(name) {
            None => 1,
            Some(d) if d.depends_on.is_empty() => 1,
            Some(d) => {
                1 + d
                    .depends_on
                    .iter()
                    .map(|dep| depth(dep, by_name, budget - 1))
                    .max()
                    .unwrap_or(0)
            }
        }
    }

    domains
        .iter()
        .map(|d| depth(&d.name, &by_name, domains.len()))
        .max()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dv(name: &str, writable: &[&str], depends_on: &[&str]) -> DomainView {
        DomainView {
            name: name.to_string(),
            writable: writable.iter().map(|s| s.to_string()).collect(),
            depends_on: depends_on.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn disjoint_globs_do_not_overlap() {
        let domains = vec![dv("a", &["src/a/**"], &[]), dv("b", &["src/b/**"], &[])];
        let inp = inputs_from_domains(&domains, false);
        assert_eq!(inp.writable_overlaps, 0);
    }

    #[test]
    fn prefix_globs_overlap() {
        let domains = vec![
            dv("broad", &["src/**"], &[]),
            dv("narrow", &["src/api/**"], &[]),
        ];
        let inp = inputs_from_domains(&domains, false);
        assert_eq!(inp.writable_overlaps, 1);
    }

    #[test]
    fn dependency_edges_are_summed() {
        let domains = vec![
            dv("a", &["src/a/**"], &[]),
            dv("b", &["src/b/**"], &["a"]),
            dv("c", &["src/c/**"], &["a", "b"]),
        ];
        let inp = inputs_from_domains(&domains, false);
        assert_eq!(inp.dependency_edges, 3);
    }

    #[test]
    fn longest_chain_is_measured() {
        // a <- b <- c : chain depth 3
        let domains = vec![
            dv("a", &["src/a/**"], &[]),
            dv("b", &["src/b/**"], &["a"]),
            dv("c", &["src/c/**"], &["b"]),
        ];
        let inp = inputs_from_domains(&domains, false);
        assert_eq!(inp.longest_chain, 3);
    }

    #[test]
    fn chain_computation_survives_a_cycle() {
        // a <-> b : the extractor must terminate, not stack-overflow.
        let domains = vec![
            dv("a", &["src/a/**"], &["b"]),
            dv("b", &["src/b/**"], &["a"]),
        ];
        let inp = inputs_from_domains(&domains, true);
        assert!(inp.longest_chain <= domains.len());
    }

    fn inp() -> EntropyInputs {
        EntropyInputs {
            domain_count: 4,
            dependency_edges: 0,
            writable_overlaps: 0,
            longest_chain: 0,
            has_static_cycle: false,
        }
    }

    #[test]
    fn clean_independent_split_is_low() {
        let s = compute(inp());
        assert_eq!(s.level(), EntropyLevel::Low);
        assert!(s.score < 25, "got {}", s.score);
    }

    #[test]
    fn single_domain_is_low_and_explains_why() {
        let s = compute(EntropyInputs {
            domain_count: 1,
            ..inp()
        });
        assert_eq!(s.level(), EntropyLevel::Low);
        assert!(s.factors.iter().any(|f| f.contains("single domain")));
    }

    #[test]
    fn static_cycle_is_always_critical() {
        let s = compute(EntropyInputs {
            has_static_cycle: true,
            ..inp()
        });
        assert_eq!(s.level(), EntropyLevel::Critical);
        assert!(s.factors.iter().any(|f| f.contains("cycle")));
    }

    #[test]
    fn overlaps_raise_the_score_monotonically() {
        let none = compute(inp()).score;
        let some = compute(EntropyInputs {
            writable_overlaps: 2,
            ..inp()
        })
        .score;
        let more = compute(EntropyInputs {
            writable_overlaps: 5,
            ..inp()
        })
        .score;
        assert!(some > none);
        assert!(more >= some);
    }

    #[test]
    fn overlap_contribution_is_capped() {
        // Even absurd overlap counts can't alone exceed the 45-pt cap, so an
        // acyclic project never lands in CRITICAL on overlaps alone.
        let s = compute(EntropyInputs {
            writable_overlaps: 100,
            ..inp()
        });
        assert!(
            s.score < 80,
            "overlap alone should not reach critical: {}",
            s.score
        );
    }

    #[test]
    fn dependency_density_contributes() {
        let sparse = compute(inp()).score;
        let dense = compute(EntropyInputs {
            dependency_edges: 8,
            ..inp()
        })
        .score;
        assert!(dense > sparse);
    }

    #[test]
    fn deep_chain_contributes_above_threshold() {
        let shallow = compute(EntropyInputs {
            longest_chain: 2,
            ..inp()
        })
        .score;
        let deep = compute(EntropyInputs {
            longest_chain: 6,
            ..inp()
        })
        .score;
        assert!(deep > shallow);
    }

    #[test]
    fn score_is_deterministic() {
        let a = compute(EntropyInputs {
            domain_count: 5,
            dependency_edges: 6,
            writable_overlaps: 2,
            longest_chain: 3,
            has_static_cycle: false,
        });
        let b = compute(EntropyInputs {
            domain_count: 5,
            dependency_edges: 6,
            writable_overlaps: 2,
            longest_chain: 3,
            has_static_cycle: false,
        });
        assert_eq!(a, b);
    }

    #[test]
    fn factors_are_never_empty() {
        // Every score, even the lowest, explains itself.
        assert!(!compute(inp()).factors.is_empty());
    }
}
