//! Deciding, for each test binary, whether it runs or is skipped, and why.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use crate::discover::TargetId;
use crate::key::KeyParts;
use crate::store::PassRecord;

/// What forces a binary to run whatever its key.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Policy {
    /// `--no-cache` was given.
    pub no_cache: bool,
    /// The binary matches an `always_run` pattern.
    pub always_run: bool,
}

/// Run or skip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// The key matches a recorded pass.
    Skip {
        /// When that pass was recorded, in Unix seconds.
        passed_at: u64,
    },
    /// The binary runs, for this reason.
    Run(RunReason),
}

/// Why a binary runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunReason {
    /// `--no-cache`.
    NoCache,
    /// An `always_run` pattern matches it.
    AlwaysRun,
    /// A part of its key could not be computed; the message says which.
    Unavailable(String),
    /// It has no recorded pass.
    NeverPassed,
    /// Its key differs from the recorded pass in these parts.
    Changed(Vec<Change>),
}

/// A part of a key that differs from the recorded pass.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Change {
    /// The tool or its key format.
    Tool,
    /// The test executable.
    Binary,
    /// A spawned binary, by its name in the key.
    Spawned(String),
    /// An input file, by its workspace-relative path.
    Input(String),
    /// An environment variable.
    Env(String),
    /// The arguments after `--`.
    Args,
}

/// The summary line's groups of run reasons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ReasonCategory {
    /// [`RunReason::Changed`].
    Changed,
    /// [`RunReason::NeverPassed`].
    NeverPassed,
    /// [`RunReason::AlwaysRun`].
    AlwaysRun,
    /// [`RunReason::NoCache`].
    NoCache,
    /// [`RunReason::Unavailable`].
    Unavailable,
}

impl ReasonCategory {
    /// The group's name in the summary line.
    pub fn label(self) -> &'static str {
        match self {
            Self::Changed => "changed",
            Self::NeverPassed => "never passed",
            Self::AlwaysRun => "always run",
            Self::NoCache => "no cache",
            Self::Unavailable => "key unavailable",
        }
    }
}

impl RunReason {
    /// The reason's group in the summary line.
    pub fn category(&self) -> ReasonCategory {
        match self {
            Self::NoCache => ReasonCategory::NoCache,
            Self::AlwaysRun => ReasonCategory::AlwaysRun,
            Self::Unavailable(_) => ReasonCategory::Unavailable,
            Self::NeverPassed => ReasonCategory::NeverPassed,
            Self::Changed(_) => ReasonCategory::Changed,
        }
    }
}

impl fmt::Display for Change {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tool => formatter.write_str("cargo-testcache changed"),
            Self::Binary => formatter.write_str("binary changed"),
            Self::Spawned(name) => write!(formatter, "spawned {name} changed"),
            Self::Input(path) => write!(formatter, "input {path} changed"),
            Self::Env(name) => write!(formatter, "env {name} changed"),
            Self::Args => formatter.write_str("arguments changed"),
        }
    }
}

impl fmt::Display for RunReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoCache => formatter.write_str("--no-cache"),
            Self::AlwaysRun => formatter.write_str("matches always_run"),
            Self::Unavailable(why) => write!(formatter, "key unavailable: {why}"),
            Self::NeverPassed => formatter.write_str("never passed"),
            Self::Changed(changes) => {
                let changes: Vec<String> = changes.iter().map(ToString::to_string).collect();
                formatter.write_str(&changes.join("; "))
            }
        }
    }
}

/// Decides whether a binary runs.
///
/// # Arguments
///
/// * `parts` - The binary's key parts, or why they could not be computed.
/// * `previous` - Its recorded pass, if any.
/// * `policy` - `--no-cache` and `always_run`.
///
/// # Returns
///
/// Skip when the key matches the recorded pass and nothing forces a run; otherwise the first
/// reason that applies, in the order `--no-cache`, `always_run`, unavailable, never passed,
/// changed.
pub fn decide(
    parts: &std::result::Result<KeyParts, String>,
    previous: Option<&PassRecord>,
    policy: Policy,
) -> Decision {
    if policy.no_cache {
        return Decision::Run(RunReason::NoCache);
    }
    if policy.always_run {
        return Decision::Run(RunReason::AlwaysRun);
    }
    let parts = match parts {
        Ok(parts) => parts,
        Err(why) => return Decision::Run(RunReason::Unavailable(why.clone())),
    };
    let Some(previous) = previous else {
        return Decision::Run(RunReason::NeverPassed);
    };
    if previous.key == parts.key() {
        return Decision::Skip {
            passed_at: previous.passed_at,
        };
    }
    let changes = changes(&previous.parts, parts);
    // Equal parts under a different key can only mean the key's encoding changed.
    let changes = if changes.is_empty() {
        vec![Change::Tool]
    } else {
        changes
    };
    Decision::Run(RunReason::Changed(changes))
}

/// The parts that differ between a recorded pass and now.
///
/// # Arguments
///
/// * `old` - The recorded parts.
/// * `new` - The current parts.
///
/// # Returns
///
/// One [`Change`] per differing part, or per differing entry of a map part.
pub fn changes(old: &KeyParts, new: &KeyParts) -> Vec<Change> {
    let mut changes = Vec::new();
    if old.tool != new.tool {
        changes.push(Change::Tool);
    }
    if old.binary != new.binary {
        changes.push(Change::Binary);
    }
    changes.extend(changed_names(&old.spawned, &new.spawned).map(Change::Spawned));
    changes.extend(changed_names(&old.inputs, &new.inputs).map(Change::Input));
    changes.extend(changed_names(&old.env, &new.env).map(Change::Env));
    if old.args != new.args {
        changes.push(Change::Args);
    }
    changes
}

/// The names that were removed, added or changed between two maps.
fn changed_names<'a, V: PartialEq>(
    old: &'a BTreeMap<String, V>,
    new: &'a BTreeMap<String, V>,
) -> impl Iterator<Item = String> + 'a {
    let removed_or_changed = old
        .iter()
        .filter(|(name, value)| new.get(*name) != Some(*value))
        .map(|(name, _)| name);
    let added = new.keys().filter(|name| !old.contains_key(*name));
    removed_or_changed.chain(added).cloned()
}

/// The record of a run, if it passed. Only a zero exit status is ever recorded.
///
/// # Arguments
///
/// * `target` - The binary's target.
/// * `parts` - Its key parts.
/// * `passed` - Whether it exited 0.
/// * `duration` - How long it ran.
/// * `passed_at` - Now, in Unix seconds.
///
/// # Returns
///
/// The record to store, or `None` for a failure.
pub fn pass_record(
    target: &TargetId,
    parts: KeyParts,
    passed: bool,
    duration: Duration,
    passed_at: u64,
) -> Option<PassRecord> {
    passed.then(|| PassRecord {
        target: target.clone(),
        key: parts.key(),
        parts,
        passed_at,
        duration_ms: u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;

    use hegel::TestCase;
    use hegel::generators as gs;

    use crate::discover::TargetKind;
    use crate::key::testing::{draw_parts, mutate_one_part};

    fn target() -> TargetId {
        TargetId {
            package_id: "p".to_owned(),
            kind: TargetKind::Lib,
            name: "p".to_owned(),
        }
    }

    fn record_of(parts: KeyParts) -> PassRecord {
        pass_record(&target(), parts, true, Duration::from_millis(5), 7)
            .expect("a pass is recorded")
    }

    // Property (design 9): with no policy in the way, a binary is skipped exactly when its
    // current parts are the ones recorded, and a run says why.
    #[hegel::test]
    fn skipped_exactly_when_the_key_is_recorded(tc: TestCase) {
        let parts = draw_parts(&tc);
        let history = tc.draw(gs::integers::<u8>().max_value(2));
        let which = tc.draw(gs::integers::<u8>().max_value(5));
        let name = tc.draw(gs::text().max_size(6));
        let previous = match history {
            0 => None,
            1 => Some(record_of(parts.clone())),
            _ => {
                let mut changed = parts.clone();
                mutate_one_part(&mut changed, which, name);
                Some(record_of(changed))
            }
        };

        let decision = decide(&Ok(parts), previous.as_ref(), Policy::default());

        match (history, decision) {
            (0, Decision::Run(RunReason::NeverPassed)) => {}
            (1, Decision::Skip { passed_at }) => assert_eq!(passed_at, 7),
            (2, Decision::Run(RunReason::Changed(changes))) => assert_eq!(changes.len(), 1),
            (history, decision) => panic!("history {history} gave {decision:?}"),
        }
    }

    // Property: --no-cache, always_run and an uncomputable key each force a run.
    #[hegel::test]
    fn policies_and_unknown_keys_force_a_run(tc: TestCase) {
        let parts = draw_parts(&tc);
        let previous = record_of(parts.clone());
        let no_cache = tc.draw(gs::booleans());
        let always_run = tc.draw(gs::booleans());
        let known = tc.draw(gs::booleans());
        let parts = if known {
            Ok(parts)
        } else {
            Err("unreadable".to_owned())
        };

        let decision = decide(
            &parts,
            Some(&previous),
            Policy {
                no_cache,
                always_run,
            },
        );

        let expected = if no_cache {
            Some(RunReason::NoCache)
        } else if always_run {
            Some(RunReason::AlwaysRun)
        } else if !known {
            Some(RunReason::Unavailable("unreadable".to_owned()))
        } else {
            None
        };
        match expected {
            Some(reason) => assert_eq!(decision, Decision::Run(reason)),
            None => assert!(matches!(decision, Decision::Skip { .. })),
        }
    }

    // Property: changes() names the one input that changed, and nothing else.
    #[hegel::test]
    fn changes_name_the_changed_input(tc: TestCase) {
        let old = draw_parts(&tc);
        let name = tc.draw(gs::text().max_size(6));
        let mut new = old.clone();
        mutate_one_part(&mut new, 3, name.clone());

        assert_eq!(changes(&old, &new), [Change::Input(name.clone())]);
        assert_eq!(
            Change::Input(name.clone()).to_string(),
            format!("input {name} changed")
        );
    }

    // Property (design 9): only a passing run produces a record, and its key is its parts' key.
    #[hegel::test]
    fn only_a_pass_is_recorded(tc: TestCase) {
        let parts = draw_parts(&tc);
        let passed = tc.draw(gs::booleans());
        let millis = tc.draw(gs::integers::<u64>());

        let record = pass_record(
            &target(),
            parts.clone(),
            passed,
            Duration::from_millis(millis),
            1,
        );

        assert_eq!(record.is_some(), passed);
        if let Some(record) = record {
            assert_eq!(record.key, parts.key());
            assert_eq!(record.duration_ms, millis);
        }
    }
}
