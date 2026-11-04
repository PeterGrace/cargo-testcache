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
    pub fn new(binaries: usize) -> Self {
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
        self.whole_run_failed |= !passed;
    }

    /// Records the doc tests' result.
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

/// A binary's label, as Cargo prints it after "Running": `unittests src/lib.rs (pkg/target)`.
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
        TargetKind::Lib | TargetKind::Bin => "unittests ",
        TargetKind::Test | TargetKind::Bench | TargetKind::Example => "",
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
pub fn format_age(age: Duration) -> String {
    let seconds = age.as_secs();
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
    /// * `message` - What is being done, such as "hashing test binaries".
    ///
    /// # Returns
    ///
    /// The bar.
    pub fn new(length: usize, message: impl Into<Cow<'static, str>>) -> Self {
        let bar = ProgressBar::new(length as u64).with_message(message);
        bar.set_style(
            ProgressStyle::with_template("{spinner} {msg} [{pos}/{len}] {elapsed}")
                .expect("the progress template is valid"),
        );
        Self(bar)
    }

    /// Prints a line above the bar.
    pub fn line(&self, text: &str) {
        self.0.suspend(|| eprintln!("{text}"));
    }

    /// Runs `work` with the bar cleared, so a test binary's output is not drawn over.
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
    }

    // Property: ages use the largest whole unit.
    #[hegel::test]
    fn ages_use_the_largest_whole_unit(tc: TestCase) {
        let seconds = tc.draw(gs::integers::<u64>().max_value(10_000_000));

        let age = format_age(Duration::from_secs(seconds));

        let expected = match seconds {
            0..60 => format!("{seconds}s"),
            60..3_600 => format!("{}m", seconds / 60),
            3_600..86_400 => format!("{}h", seconds / 3_600),
            _ => format!("{}d", seconds / 86_400),
        };
        assert_eq!(age, expected);
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

        let unit = matches!(kind, TargetKind::Lib | TargetKind::Bin);
        assert_eq!(label.starts_with("unittests "), unit, "{label}");
        assert!(
            label.contains(&format!("src{}lib.rs", std::path::MAIN_SEPARATOR)),
            "{label}"
        );
        assert!(label.ends_with("(a/t)"), "{label}");
    }
}
