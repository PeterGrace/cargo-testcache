//! What a run prints on stderr (stdout is the tests' own output): a line per binary in Cargo's
//! style, a progress bar while hashing, and the closing summary line.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::time::Duration;

use indicatif::{ProgressBar, ProgressStyle};

use crate::discover::{TargetKind, TestBinary};
use crate::plan::ReasonCategory;

/// Counts for the summary line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tally {
    binaries: usize,
    skipped: usize,
    failed: usize,
    run: BTreeMap<ReasonCategory, usize>,
    /// A whole-workspace `cargo test` (the via-cargo shortcut) failed; which binary is unknown.
    whole_run_failed: bool,
    doc_tests_failed: bool,
}

impl Tally {
    /// An empty tally for this many binaries.
    ///
    /// # Arguments
    ///
    /// * `binaries` - How many test binaries the build produced.
    ///
    /// # Returns
    ///
    /// A tally with every count at zero.
    pub fn new(binaries: usize) -> Self {
        // `..Self::default()` is like Python's `{**defaults, "binaries": binaries}`: the named
        // field is set and every other field comes from `Default`.
        Self {
            binaries,
            ..Self::default()
        }
    }

    /// Counts a skipped binary.
    pub fn skipped(&mut self) {
        self.skipped += 1;
    }

    /// Counts a binary that ran.
    ///
    /// # Arguments
    ///
    /// * `category` - Why it ran.
    /// * `passed` - Whether it passed.
    pub fn ran(&mut self, category: ReasonCategory, passed: bool) {
        *self.run.entry(category).or_default() += 1;
        if !passed {
            self.failed += 1;
        }
    }

    /// Counts binaries that ran together in one `cargo test`.
    ///
    /// # Arguments
    ///
    /// * `categories` - Why each ran.
    /// * `passed` - Whether the whole run passed.
    pub fn ran_together(
        &mut self,
        categories: impl IntoIterator<Item = ReasonCategory>,
        passed: bool,
    ) {
        for category in categories {
            *self.run.entry(category).or_default() += 1;
        }
        // `|=` on a bool is `x = x or (not passed)`: once set, the flag stays set.
        self.whole_run_failed |= !passed;
    }

    /// Records the doc tests' result.
    ///
    /// # Arguments
    ///
    /// * `passed` - Whether the doc tests passed.
    pub fn doc_tests(&mut self, passed: bool) {
        self.doc_tests_failed |= !passed;
    }

    /// Whether anything failed.
    pub fn any_failed(&self) -> bool {
        self.failed > 0 || self.whole_run_failed || self.doc_tests_failed
    }
}

impl fmt::Display for Tally {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let noun = if self.binaries == 1 {
            "binary"
        } else {
            "binaries"
        };
        let run: usize = self.run.values().sum();
        write!(formatter, "testcache: {} {noun}, {run} run", self.binaries)?;
        if run > 0 {
            let reasons: Vec<String> = self
                .run
                .iter()
                .map(|(category, count)| format!("{count} {}", category.label()))
                .collect();
            write!(formatter, " ({})", reasons.join(", "))?;
        }
        write!(formatter, ", {} skipped", self.skipped)?;
        if self.failed > 0 {
            write!(formatter, "; {} failed", self.failed)?;
        }
        if self.whole_run_failed {
            formatter.write_str("; cargo test failed")?;
        }
        if self.doc_tests_failed {
            formatter.write_str("; doc tests failed")?;
        }
        Ok(())
    }
}

/// A Cargo-style label such as `unittests src/lib.rs (pkg/target)`. Cargo prints the executable's
/// path where the parentheses here name `pkg/target`.
///
/// # Arguments
///
/// * `binary` - The binary.
/// * `package_name` - Its package's name.
/// * `package_root` - Its package's directory, to shorten the source path.
///
/// # Returns
///
/// The label.
pub fn label(binary: &TestBinary, package_name: &str, package_root: &Path) -> String {
    let source = binary
        .src_path
        .strip_prefix(package_root)
        .unwrap_or(&binary.src_path);
    let prefix = match binary.id.kind {
        TargetKind::Lib | TargetKind::Bin | TargetKind::Example => "unittests ",
        TargetKind::Test | TargetKind::Bench => "",
    };
    format!(
        "{prefix}{} ({package_name}/{})",
        source.display(),
        binary.id.name
    )
}

/// One status line: a right-aligned verb as Cargo prints them, the label, and a detail.
///
/// # Arguments
///
/// * `verb` - Such as "Skipped".
/// * `label` - From [`label`].
/// * `detail` - Such as the reason it runs.
///
/// # Returns
///
/// The line.
pub fn status_line(verb: &str, label: &str, detail: Option<&str>) -> String {
    match detail {
        Some(detail) => format!("{verb:>12} {label}: {detail}"),
        None => format!("{verb:>12} {label}"),
    }
}

/// An age in its largest whole unit: `42s`, `5m`, `3h`, `2d`.
///
/// # Arguments
///
/// * `age` - The age to show; sub-second parts are dropped.
///
/// # Returns
///
/// The age as a whole number followed by `s`, `m`, `h` or `d`.
pub fn format_age(age: Duration) -> String {
    let seconds = age.as_secs();
    // `0..60` is a half-open range, like Python's `range(0, 60)`: it excludes 60.
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3_600 => format!("{}m", seconds / 60),
        3_600..86_400 => format!("{}h", seconds / 3_600),
        _ => format!("{}d", seconds / 86_400),
    }
}

/// A progress bar on stderr. indicatif hides it when stderr is not a terminal, so piped output
/// gets only the plain lines printed through [`Progress::line`].
#[derive(Debug, Clone)]
pub struct Progress(ProgressBar);

impl Progress {
    /// Starts a bar.
    ///
    /// # Arguments
    ///
    /// * `length` - How many steps.
    /// * `message` - What is being done, such as "hashing test binaries". `impl Into<Cow<..>>`
    ///   accepts a `&'static str` (borrowed, no allocation) or a `String` (owned) as one
    ///   parameter; think of it as "str or str-like".
    ///
    /// # Returns
    ///
    /// The bar.
    pub fn new(length: usize, message: impl Into<Cow<'static, str>>) -> Self {
        let bar = ProgressBar::new(length as u64).with_message(message);
        bar.set_style(
            ProgressStyle::with_template("{spinner} {msg} [{wide_bar}] {pos}/{len} {elapsed}")
                .expect("the progress template is valid"),
        );
        // A hidden bar (stderr is not a terminal) has no ticker thread worth starting.
        if !bar.is_hidden() {
            bar.enable_steady_tick(Duration::from_millis(100));
        }
        Self(bar)
    }

    /// Changes the message, such as to the binary now running.
    ///
    /// # Arguments
    ///
    /// * `message` - The new message; a `&'static str` or a `String`.
    pub fn set_message(&self, message: impl Into<Cow<'static, str>>) {
        self.0.set_message(message);
    }

    /// Prints a line above the bar.
    ///
    /// # Arguments
    ///
    /// * `text` - The line, without a trailing newline.
    pub fn line(&self, text: &str) {
        self.0.suspend(|| eprintln!("{text}"));
    }

    /// Runs `work` with the bar cleared, so a test binary's output is not drawn over.
    ///
    /// # Arguments
    ///
    /// * `work` - The closure to run; like a Python callable passed in.
    ///
    /// # Returns
    ///
    /// Whatever `work` returns.
    pub fn suspended<R>(&self, work: impl FnOnce() -> R) -> R {
        self.0.suspend(work)
    }

    /// Advances the bar by one step. Safe to call from several threads.
    pub fn inc(&self) {
        self.0.inc(1);
    }

    /// Removes the bar.
    pub fn finish(&self) {
        self.0.finish_and_clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use hegel::TestCase;
    use hegel::generators::{self as gs, Generator};

    use crate::discover::TargetId;

    const CATEGORIES: [ReasonCategory; 5] = [
        ReasonCategory::Changed,
        ReasonCategory::NeverPassed,
        ReasonCategory::AlwaysRun,
        ReasonCategory::NoCache,
        ReasonCategory::Unavailable,
    ];

    // Property: the summary line states the counts, lists reasons only when something ran, and
    // mentions failures only when there were some.
    #[hegel::test]
    fn the_summary_line_states_the_counts(tc: TestCase) {
        let events = tc.draw(
            gs::vecs(gs::optional(gs::tuples!(
                gs::sampled_from(&CATEGORIES).print_as_debug(),
                gs::booleans()
            )))
            .max_size(8),
        );
        let mut tally = Tally::new(events.len());
        for event in &events {
            match event {
                None => tally.skipped(),
                Some((category, passed)) => tally.ran(*category, *passed),
            }
        }

        let line = tally.to_string();

        let run = events.iter().flatten().count();
        let failed = events
            .iter()
            .flatten()
            .filter(|(_, passed)| !passed)
            .count();
        let noun = if events.len() == 1 {
            "binary"
        } else {
            "binaries"
        };
        assert!(
            line.starts_with(&format!("testcache: {} {noun}, {run} run", events.len())),
            "{line}"
        );
        assert!(
            line.contains(&format!("{} skipped", events.len() - run)),
            "{line}"
        );
        assert_eq!(line.contains('('), run > 0, "{line}");
        assert_eq!(line.contains("failed"), failed > 0, "{line}");
        assert_eq!(tally.any_failed(), failed > 0);
        let mut counts: BTreeMap<ReasonCategory, usize> = BTreeMap::new();
        for (category, _) in events.iter().flatten() {
            *counts.entry(*category).or_default() += 1;
        }
        let expected: Vec<String> = counts
            .iter()
            .map(|(category, count)| format!("{count} {}", category.label()))
            .collect();
        let group = line
            .split_once('(')
            .and_then(|(_, rest)| rest.split_once(')'));
        assert_eq!(
            group.map(|(reasons, _)| reasons.to_owned()),
            (run > 0).then(|| expected.join(", ")),
            "{line}"
        );
    }

    // Property: runs counted together add the cargo-test failure suffix, and doc tests add theirs.
    #[hegel::test]
    fn together_and_doc_failures_show_in_the_line(tc: TestCase) {
        let categories =
            tc.draw(gs::vecs(gs::sampled_from(&CATEGORIES).print_as_debug()).max_size(6));
        let together_passed = tc.draw(gs::booleans());
        let doc_passed = tc.draw(gs::booleans());
        let mut tally = Tally::new(categories.len());
        tally.ran_together(categories.iter().copied(), together_passed);
        tally.doc_tests(doc_passed);

        let line = tally.to_string();

        assert_eq!(
            line.contains("; cargo test failed"),
            !together_passed,
            "{line}"
        );
        assert_eq!(line.contains("; doc tests failed"), !doc_passed, "{line}");
        assert_eq!(tally.any_failed(), !together_passed || !doc_passed);
    }

    // Property: the verb is right-aligned to 12 columns and a detail follows only when given.
    #[hegel::test]
    fn status_lines_align_the_verb(tc: TestCase) {
        let verb = tc.draw(gs::text().alphabet("abcdefghij").max_size(12));
        let label = tc.draw(gs::text().alphabet("klmnop").min_size(1).max_size(10));
        let detail = tc.draw(gs::optional(
            gs::text().alphabet("qrstuv").min_size(1).max_size(10),
        ));

        let line = status_line(&verb, &label, detail.as_deref());

        let padding = 12 - verb.len();
        let head = format!("{}{verb} {label}", " ".repeat(padding));
        match &detail {
            Some(detail) => assert_eq!(line, format!("{head}: {detail}")),
            None => assert_eq!(line, head),
        }
    }

    // Property: an age is a whole number of the largest unit that fits, rounded down.
    #[hegel::test]
    fn ages_use_the_largest_whole_unit(tc: TestCase) {
        let seconds = tc.draw(gs::integers::<u64>().max_value(10_000_000));

        let age = format_age(Duration::from_secs(seconds));

        let (digits, unit) = age.split_at(age.len() - 1);
        let count: u64 = digits.parse().expect("a number precedes the unit");
        let (unit_seconds, next_unit_seconds) = match unit {
            "s" => (1, Some(60)),
            "m" => (60, Some(3_600)),
            "h" => (3_600, Some(86_400)),
            "d" => (86_400, None),
            other => panic!("unknown unit {other}"),
        };
        assert!(
            count * unit_seconds <= seconds && seconds < (count + 1) * unit_seconds,
            "{age}"
        );
        if let Some(next) = next_unit_seconds {
            assert!(count < next / unit_seconds, "{age}");
        }
        if unit != "s" {
            assert!(count >= 1, "{age}");
        }
    }

    // Property: unit tests are labelled `unittests <src>`, other targets by their source, and
    // every label names the package and target.
    #[hegel::test]
    fn labels_look_like_cargos(tc: TestCase) {
        let kind = tc.draw(
            gs::sampled_from(&[
                TargetKind::Lib,
                TargetKind::Bin,
                TargetKind::Test,
                TargetKind::Bench,
                TargetKind::Example,
            ])
            .print_as_debug(),
        );
        let binary = TestBinary {
            id: TargetId {
                package_id: "id".to_owned(),
                kind,
                name: "t".to_owned(),
            },
            src_path: PathBuf::from("/ws/a/src/lib.rs"),
            executable: PathBuf::from("/ws/target/debug/deps/t-1"),
        };

        let label = label(&binary, "a", Path::new("/ws/a"));

        let unit = matches!(
            kind,
            TargetKind::Lib | TargetKind::Bin | TargetKind::Example
        );
        assert_eq!(label.starts_with("unittests "), unit, "{label}");
        assert!(
            label.contains(&format!("src{}lib.rs", std::path::MAIN_SEPARATOR)),
            "{label}"
        );
        assert!(label.ends_with("(a/t)"), "{label}");
    }
}
