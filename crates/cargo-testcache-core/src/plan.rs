//! Deciding, for each test binary, whether it runs or is skipped, and why.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use crate::discover::TargetId;
use crate::key::{Digest, KeyParts, PartResult};
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
    /// A spawned binary, by its name in the key ([`KeyParts::spawned`]).
    Spawned(String),
    /// An input file, by its workspace-relative path.
    Input(String),
    /// An environment variable, and how its entry in the key differs.
    Env {
        /// The variable's name.
        name: String,
        /// How it differs.
        change: EnvChange,
    },
    /// The arguments after `--`.
    Args,
    /// Cargo's configuration files or `--config` values.
    CargoConfig,
}

/// How a variable's entry in the key differs from the recorded pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EnvChange {
    /// Keyed both times, with a different value (or set one time and unset the other).
    Value,
    /// Not keyed before: newly declared, or a `RUST_TEST_TIME_*` variable newly set. `set` is
    /// whether it is set now, so an unset one is not mistaken for a change to the environment.
    Added {
        /// Whether the variable is set now.
        set: bool,
    },
    /// Keyed before, not now.
    Removed,
}

impl EnvChange {
    /// How a variable's entry differs between two keys.
    ///
    /// # Arguments
    ///
    /// * `old` - Its entry in the recorded key, if it had one: `Some(None)` is keyed but unset.
    /// * `new` - Its entry in the current key, likewise.
    ///
    /// # Returns
    ///
    /// The difference, or `None` when the entries are equal.
    pub fn between(old: Option<&Option<Digest>>, new: Option<&Option<Digest>>) -> Option<Self> {
        match (old, new) {
            (None, None) => None,
            (None, Some(value)) => Some(Self::Added {
                set: value.is_some(),
            }),
            (Some(_), None) => Some(Self::Removed),
            (Some(old), Some(new)) => (old != new).then_some(Self::Value),
        }
    }
}

/// A spawned binary's key name ([`KeyParts::spawned`]) split into the binary and the target
/// whose `targets.<name>` table declares it, if one does.
///
/// # Arguments
///
/// * `name` - `bin <name>`, `spawns <path>` or `target <target> spawns <path>`.
///
/// # Returns
///
/// `(x, None)` for `bin x` and `spawns x`, `(x, Some(t))` for `target t spawns x`, and any
/// other name whole, with no target.
fn spawned_parts(name: &str) -> (&str, Option<&str>) {
    // Cargo target names hold no spaces, so the first ` spawns ` ends the target's name.
    if let Some((target, path)) = name
        .strip_prefix("target ")
        .and_then(|rest| rest.split_once(" spawns "))
    {
        return (path, Some(target));
    }
    let binary = name
        .strip_prefix("bin ")
        .or_else(|| name.strip_prefix("spawns "))
        .unwrap_or(name);
    (binary, None)
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
    ///
    /// # Returns
    ///
    /// Such as `changed` or `never passed`.
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
    ///
    /// # Returns
    ///
    /// The [`ReasonCategory`] of this variant.
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
            // A target's spawn names its target the way an input names its declaring table.
            Self::Spawned(name) => match spawned_parts(name) {
                (binary, Some(target)) => {
                    write!(
                        formatter,
                        "spawned binary {binary} changed (target `{target}`)"
                    )
                }
                (binary, None) => write!(formatter, "spawned binary {binary} changed"),
            },
            Self::Input(path) => write!(formatter, "input {path} changed"),
            Self::Env { name, change } => match change {
                EnvChange::Value => write!(formatter, "env {name} changed"),
                EnvChange::Added { set: true } => write!(formatter, "env {name} newly keyed, set"),
                EnvChange::Added { set: false } => {
                    write!(formatter, "env {name} newly declared, unset")
                }
                EnvChange::Removed => write!(formatter, "env {name} no longer keyed"),
            },
            Self::Args => formatter.write_str("arguments changed"),
            Self::CargoConfig => formatter.write_str("cargo configuration changed"),
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
            Self::Changed(changes) => formatter.write_str(&join_changes(changes, |_| None)),
        }
    }
}

impl RunReason {
    /// The reason as [`Display`](fmt::Display) words it, with a note added after any change
    /// the caller knows more about (such as which table declares a changed input).
    ///
    /// # Arguments
    ///
    /// * `note` - The note for a change, or `None` to leave it as it is.
    ///
    /// # Returns
    ///
    /// The text: a noted change reads `input x changed (note)`.
    pub fn describe(&self, note: impl Fn(&Change) -> Option<String>) -> String {
        match self {
            Self::Changed(changes) => join_changes(changes, note),
            Self::NoCache | Self::AlwaysRun | Self::Unavailable(_) | Self::NeverPassed => {
                self.to_string()
            }
        }
    }
}

/// The changes, each with its note if it has one, separated by `; `.
fn join_changes(changes: &[Change], note: impl Fn(&Change) -> Option<String>) -> String {
    changes
        .iter()
        .map(|change| match note(change) {
            Some(note) => format!("{change} ({note})"),
            None => change.to_string(),
        })
        .collect::<Vec<_>>()
        .join("; ")
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
    parts: &PartResult<KeyParts>,
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
    // `let ... else` is a pattern match that must succeed; the `else` block must leave the
    // function (here by returning), like an early `if x is None: return` in Python.
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
    changes.extend(changed_names(&old.env, &new.env).filter_map(|name| {
        let change = EnvChange::between(old.env.get(&name), new.env.get(&name))?;
        Some(Change::Env { name, change })
    }));
    if old.args != new.args {
        changes.push(Change::Args);
    }
    if old.cargo_config != new.cargo_config {
        changes.push(Change::CargoConfig);
    }
    changes
}

/// The names that were removed, added or changed between two maps.
fn changed_names<'a, V: PartialEq>(
    old: &'a BTreeMap<String, V>,
    new: &'a BTreeMap<String, V>,
) -> impl Iterator<Item = String> + 'a {
    // `impl Iterator + 'a` returns some lazy iterator (like a Python generator) that borrows
    // from `old` and `new`; `'a` says it may not outlive them.
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
    // `bool::then` runs the closure only when `passed` is true and gives `Some(result)`, else
    // `None`: Python's `Record(...) if passed else None`.
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
    use crate::key::testing::{PART_COUNT, draw_parts, mutate_one_part};

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

    /// The one change `mutate_one_part` makes for `which` (below `PART_COUNT`), named `name`,
    /// going from `old` to `new` (one of which it mutated).
    fn expected_change(which: u8, name: &str, old: &KeyParts, new: &KeyParts) -> Change {
        match which {
            0 => Change::Tool,
            1 => Change::Binary,
            2 => Change::Spawned(name.to_owned()),
            3 => Change::Input(name.to_owned()),
            4 => Change::Env {
                name: name.to_owned(),
                change: EnvChange::between(old.env.get(name), new.env.get(name))
                    .expect("mutate_one_part changes the entry"),
            },
            5 => Change::Args,
            _ => Change::CargoConfig,
        }
    }

    // Property (design 9): with no policy in the way, a binary is skipped exactly when its
    // current parts are the ones recorded, and a run says which single part changed.
    #[hegel::test]
    fn skipped_exactly_when_the_key_is_recorded(tc: TestCase) {
        let parts = draw_parts(&tc);
        let history = tc.draw(gs::integers::<u8>().max_value(2));
        let which = tc.draw(gs::integers::<u8>().max_value(PART_COUNT - 1));
        let name = tc.draw(gs::text().max_size(6));
        let previous = match history {
            0 => None,
            1 => Some(record_of(parts.clone())),
            _ => {
                let mut changed = parts.clone();
                mutate_one_part(&mut changed, which, name.clone());
                Some(record_of(changed))
            }
        };
        // Only history 2 records changed parts: they are the old ones, the drawn parts the new.
        let expected_change = previous
            .as_ref()
            .filter(|_| history == 2)
            .map(|record| expected_change(which, &name, &record.parts, &parts));

        let decision = decide(&Ok(parts), previous.as_ref(), Policy::default());

        match (history, decision) {
            (0, Decision::Run(RunReason::NeverPassed)) => {}
            (1, Decision::Skip { passed_at }) => assert_eq!(passed_at, 7),
            (2, Decision::Run(RunReason::Changed(changes))) => {
                assert_eq!(Some(changes), expected_change.map(|change| vec![change]));
            }
            (history, decision) => panic!("history {history} gave {decision:?}"),
        }
    }

    // Property: --no-cache, always_run and an uncomputable key each force a run, in that order,
    // then a missing record, then the key comparison.
    #[hegel::test]
    fn policies_and_unknown_keys_force_a_run(tc: TestCase) {
        let parts = draw_parts(&tc);
        let history = tc.draw(gs::integers::<u8>().max_value(2));
        let which = tc.draw(gs::integers::<u8>().max_value(PART_COUNT - 1));
        let name = tc.draw(gs::text().max_size(6));
        let previous = match history {
            0 => None,
            1 => Some(record_of(parts.clone())),
            _ => {
                let mut changed = parts.clone();
                mutate_one_part(&mut changed, which, name.clone());
                Some(record_of(changed))
            }
        };
        // Only history 2 records changed parts: they are the old ones, the drawn parts the new.
        let expected_change = previous
            .as_ref()
            .filter(|_| history == 2)
            .map(|record| expected_change(which, &name, &record.parts, &parts));
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
            previous.as_ref(),
            Policy {
                no_cache,
                always_run,
            },
        );

        let expected = if no_cache {
            Decision::Run(RunReason::NoCache)
        } else if always_run {
            Decision::Run(RunReason::AlwaysRun)
        } else if !known {
            Decision::Run(RunReason::Unavailable("unreadable".to_owned()))
        } else {
            match history {
                0 => Decision::Run(RunReason::NeverPassed),
                1 => Decision::Skip { passed_at: 7 },
                _ => Decision::Run(RunReason::Changed(expected_change.into_iter().collect())),
            }
        };
        assert_eq!(decision, expected);
    }

    // Property: a record whose stored key disagrees with its parts is corrupt; it must run
    // (blaming the tool, since the parts match) and never skip.
    #[hegel::test]
    fn a_corrupt_record_runs_never_skips(tc: TestCase) {
        let parts = draw_parts(&tc);
        let mut record = record_of(parts.clone());
        let corrupt = Digest::of_bytes(b"corrupt");
        tc.assume(corrupt != record.key);
        record.key = corrupt;

        let decision = decide(&Ok(parts), Some(&record), Policy::default());

        assert_eq!(
            decision,
            Decision::Run(RunReason::Changed(vec![Change::Tool]))
        );
    }

    // Property: changes() names the one part that changed, and nothing else.
    #[hegel::test]
    fn changes_name_the_changed_part(tc: TestCase) {
        let old = draw_parts(&tc);
        let which = tc.draw(gs::integers::<u8>().max_value(PART_COUNT - 1));
        let name = tc.draw(gs::text().max_size(6));
        let mut new = old.clone();
        mutate_one_part(&mut new, which, name.clone());

        assert_eq!(
            changes(&old, &new),
            [expected_change(which, &name, &old, &new)]
        );
        assert_eq!(
            Change::Input(name.clone()).to_string(),
            format!("input {name} changed")
        );
        assert_eq!(
            Change::CargoConfig.to_string(),
            "cargo configuration changed"
        );
    }

    // Property: a variable's entry differs exactly when the two entries are unequal, and the
    // explanation says whether the key gained, lost or changed it. One unset both times but
    // newly declared reads as declared, not as a change to the environment.
    #[hegel::test]
    fn env_changes_say_how_the_entry_differs(tc: TestCase) {
        let draw_entry = || {
            tc.draw(gs::optional(gs::optional(gs::booleans())))
                .map(|value| value.map(|byte| Digest::of_bytes(&[u8::from(byte)])))
        };
        let old = draw_entry();
        let new = draw_entry();
        let name = tc.draw(gs::text().alphabet("ABC_").min_size(1).max_size(4));

        let change = EnvChange::between(old.as_ref(), new.as_ref());

        assert_eq!(change.is_none(), old == new);
        let Some(change) = change else { return };
        let text = Change::Env {
            name: name.clone(),
            change,
        }
        .to_string();
        let expected = match (old, new) {
            (None, Some(None)) => format!("env {name} newly declared, unset"),
            (None, Some(Some(_))) => format!("env {name} newly keyed, set"),
            (Some(_), None) => format!("env {name} no longer keyed"),
            (Some(_), Some(_)) => format!("env {name} changed"),
            (None, None) => unreachable!("equal entries are no change"),
        };
        assert_eq!(text, expected);
    }

    // Property: a spawned binary is explained by its own name, never by its key prefix, and a
    // target-level one names its target the way an input names its declaring table.
    #[hegel::test]
    fn spawned_binaries_are_explained_by_name(tc: TestCase) {
        let binary = tc.draw(gs::text().alphabet("ab/ .").min_size(1).max_size(6));
        let target = tc.draw(gs::text().alphabet("tu_-").min_size(1).max_size(4));
        let level = tc.draw(gs::integers::<u8>().max_value(2));
        let name = match level {
            0 => format!("bin {binary}"),
            1 => format!("spawns {binary}"),
            _ => format!("target {target} spawns {binary}"),
        };

        let text = Change::Spawned(name).to_string();

        let expected = match level {
            0 | 1 => format!("spawned binary {binary} changed"),
            _ => format!("spawned binary {binary} changed (target `{target}`)"),
        };
        assert_eq!(text, expected);
    }

    // Property: removing one input is reported as that input changing, and nothing else.
    #[hegel::test]
    fn a_removed_input_is_a_change(tc: TestCase) {
        let old = draw_parts(&tc);
        tc.assume(!old.inputs.is_empty());
        let index = tc.draw(gs::integers::<usize>().max_value(old.inputs.len() - 1));
        let removed = old
            .inputs
            .keys()
            .nth(index)
            .expect("index is in range")
            .clone();
        let mut new = old.clone();
        new.inputs.remove(&removed);

        assert_eq!(changes(&old, &new), [Change::Input(removed)]);
    }

    // Property (design 9): only a passing run produces a record, and its key is its parts' key.
    // The duration saturates at u64::MAX instead of wrapping.
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

    // Property: a duration too long for u64 milliseconds is recorded as u64::MAX.
    #[hegel::test]
    fn an_enormous_duration_saturates(tc: TestCase) {
        let parts = draw_parts(&tc);

        let record =
            pass_record(&target(), parts, true, Duration::MAX, 1).expect("a pass is recorded");

        assert_eq!(record.duration_ms, u64::MAX);
    }

    // Property: describing a reason adds each note after its own change and nothing else, and
    // without notes it reads as the reason's Display.
    #[hegel::test]
    fn describe_adds_notes_to_the_changes_they_belong_to(tc: TestCase) {
        let names = tc.draw(gs::vecs(gs::text().alphabet("ab").max_size(4)).max_size(4));
        let note = tc.draw(gs::text().alphabet("xyz ").min_size(1).max_size(6));
        let mut changes: Vec<Change> = names.iter().cloned().map(Change::Input).collect();
        changes.push(Change::Binary);
        let reason = RunReason::Changed(changes);
        let noted = |change: &Change| matches!(change, Change::Input(_)).then(|| note.clone());

        let described = reason.describe(noted);

        let mut expected: Vec<String> = names
            .iter()
            .map(|name| format!("input {name} changed ({note})"))
            .collect();
        expected.push("binary changed".to_owned());
        assert_eq!(described, expected.join("; "));
        assert_eq!(reason.describe(|_| None), reason.to_string());
        assert_eq!(
            RunReason::NeverPassed.describe(noted),
            RunReason::NeverPassed.to_string()
        );
    }
}
