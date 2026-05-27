//! Locate a `pi` binary on a remote host.
//!
//! The resolver enumerates a deterministic candidate list, probing each
//! path in order. The first hit wins; if none of the explicit candidates
//! resolve, the resolver falls back to a `$PATH` lookup for `pi`.
//!
//! Probe order (spec, see `VAL-REM-001`):
//!   1. `~/.local/bin/pi`
//!   2. `/opt/homebrew/bin/pi`
//!   3. `/usr/local/bin/pi`
//!   4. `/usr/bin/pi`
//!   5. `$PATH:pi` (shell `command -v pi`)
//!
//! The order is exercised by both an explicit unit test and the live SSH
//! resolver. To keep tests hermetic, candidate generation accepts a
//! [`Probe`] trait so the file-existence and `$PATH` lookup steps can be
//! injected in unit tests.

/// A single resolver candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PiBinaryCandidate {
    /// An explicit absolute path (after `$HOME` expansion).
    Explicit(String),
    /// Fallback `$PATH` lookup for the named binary.
    PathLookup(&'static str),
}

impl PiBinaryCandidate {
    pub(crate) fn display(&self) -> String {
        match self {
            Self::Explicit(path) => path.clone(),
            Self::PathLookup(name) => format!("$PATH:{name}"),
        }
    }
}

/// Trait abstracting the filesystem / `$PATH` probes so unit tests can
/// inject a virtual filesystem.
pub(crate) trait Probe {
    /// Expand a leading `~/` to the user's home directory, when known.
    fn expand_home(&self, path: &str) -> String;
    /// Return `true` if `path` resolves to an executable file.
    fn is_executable(&self, path: &str) -> bool;
    /// Return the absolute path of the first `name` found on `$PATH`,
    /// or `None` if no such entry exists.
    fn path_lookup(&self, name: &str) -> Option<String>;
}

/// The canonical (spec-defined) explicit probe locations, in order. Kept
/// as a `const` so tests can compare against the same source of truth.
pub(crate) const PI_BINARY_EXPLICIT_PATHS: &[&str] = &[
    "~/.local/bin/pi",
    "/opt/homebrew/bin/pi",
    "/usr/local/bin/pi",
    "/usr/bin/pi",
];

/// The shell binary name used for the `$PATH` fallback.
pub(crate) const PI_BINARY_NAME: &str = "pi";

/// Produce the resolver's candidate vector in the exact spec order. This
/// is the same list used by the SSH resolver and by the unit test; it
/// does not perform any I/O on its own, callers must drive it through a
/// [`Probe`].
pub(crate) fn pi_binary_candidates() -> Vec<PiBinaryCandidate> {
    let mut candidates: Vec<PiBinaryCandidate> = PI_BINARY_EXPLICIT_PATHS
        .iter()
        .map(|p| PiBinaryCandidate::Explicit((*p).to_string()))
        .collect();
    candidates.push(PiBinaryCandidate::PathLookup(PI_BINARY_NAME));
    candidates
}

/// Walk the candidate list and return the first hit, or `None`.
pub(crate) fn resolve_pi_binary<P: Probe>(probe: &P) -> Option<String> {
    for candidate in pi_binary_candidates() {
        match candidate {
            PiBinaryCandidate::Explicit(raw) => {
                let expanded = probe.expand_home(&raw);
                if probe.is_executable(&expanded) {
                    return Some(expanded);
                }
            }
            PiBinaryCandidate::PathLookup(name) => {
                if let Some(found) = probe.path_lookup(name) {
                    return Some(found);
                }
            }
        }
    }
    None
}

#[cfg(test)]
#[test]
fn probe_order_matches_spec() {
    let candidates = pi_binary_candidates();
    let rendered: Vec<String> = candidates.iter().map(PiBinaryCandidate::display).collect();
    // Print the candidate vector so `--nocapture` runs surface it for
    // the validation contract evidence.
    println!("pi_binary candidate vector:");
    for (idx, entry) in rendered.iter().enumerate() {
        println!("  [{idx}] {entry}");
    }

    assert_eq!(
        rendered,
        vec![
            "~/.local/bin/pi".to_string(),
            "/opt/homebrew/bin/pi".to_string(),
            "/usr/local/bin/pi".to_string(),
            "/usr/bin/pi".to_string(),
            "$PATH:pi".to_string(),
        ],
        "candidate order must match VAL-REM-001 spec"
    );

    assert!(matches!(
        candidates.last().expect("non-empty candidates"),
        PiBinaryCandidate::PathLookup("pi")
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    struct FakeProbe {
        home: &'static str,
        executables: HashSet<String>,
        path_hits: Vec<(&'static str, String)>,
    }

    impl Probe for FakeProbe {
        fn expand_home(&self, path: &str) -> String {
            if let Some(rest) = path.strip_prefix("~/") {
                format!("{}/{}", self.home, rest)
            } else {
                path.to_string()
            }
        }
        fn is_executable(&self, path: &str) -> bool {
            self.executables.contains(path)
        }
        fn path_lookup(&self, name: &str) -> Option<String> {
            self.path_hits
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, p)| p.clone())
        }
    }

    impl FakeProbe {
        fn empty() -> Self {
            Self {
                home: "/home/test",
                executables: HashSet::new(),
                path_hits: Vec::new(),
            }
        }
    }

    #[test]
    fn resolver_picks_first_existing_candidate() {
        let mut probe = FakeProbe::empty();
        probe
            .executables
            .insert("/usr/local/bin/pi".to_string());
        probe
            .executables
            .insert("/home/test/.local/bin/pi".to_string());

        let resolved = resolve_pi_binary(&probe);
        assert_eq!(resolved.as_deref(), Some("/home/test/.local/bin/pi"));
    }

    #[test]
    fn resolver_falls_back_to_path_lookup() {
        let mut probe = FakeProbe::empty();
        probe
            .path_hits
            .push(("pi", "/opt/custom/bin/pi".to_string()));

        let resolved = resolve_pi_binary(&probe);
        assert_eq!(resolved.as_deref(), Some("/opt/custom/bin/pi"));
    }

    #[test]
    fn resolver_returns_none_when_nothing_matches() {
        let probe = FakeProbe::empty();
        assert!(resolve_pi_binary(&probe).is_none());
    }
}
