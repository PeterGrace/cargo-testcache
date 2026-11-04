//! Splitting the command line into this tool's own options, Cargo's arguments, and the test
//! binaries' arguments, and answering questions about Cargo's.

use std::ffi::{OsStr, OsString};

/// The first argument Cargo passes when it runs `cargo testcache` as `cargo-testcache testcache`.
pub const SUBCOMMAND_NAME: &str = "testcache";

/// Package selection options that take a value.
const PACKAGE_SELECTION_WITH_VALUE: [&str; 3] = ["--package", "-p", "--exclude"];
/// Package selection switches.
const PACKAGE_SELECTION: [&str; 2] = ["--workspace", "--all"];
/// Target selection options that take a value.
const TARGET_SELECTION_WITH_VALUE: [&str; 4] = ["--bin", "--example", "--test", "--bench"];
/// Target selection switches. Any target selection turns `cargo test`'s doc tests off.
const TARGET_SELECTION: [&str; 6] = [
    "--lib",
    "--bins",
    "--examples",
    "--tests",
    "--benches",
    "--all-targets",
];
/// Arguments that make the cache pointless, so `cargo test` runs unchanged.
const PASSTHROUGH: [&str; 2] = ["--no-run", "--doc"];
/// Arguments this tool sets itself.
const UNSUPPORTED: [&str; 1] = ["--message-format"];

/// Whether one of the tool's own options takes a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arity {
    /// A flag such as `--explain`.
    Switch,
    /// An option such as `--log-format json` (or `--log-format=json`).
    Value,
}

/// The command line, split into its three groups.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SplitArgs {
    /// The tool's own subcommand and options, for clap.
    pub own: Vec<OsString>,
    /// Arguments for `cargo test`.
    pub cargo: CargoArgs,
    /// Arguments for every test binary (after `--`).
    pub test: Vec<OsString>,
}

/// Splits the command line.
///
/// # Arguments
///
/// * `args` - The arguments after the program name. A leading `testcache` (what Cargo passes
///   when run as `cargo testcache`) is dropped.
/// * `subcommands` - The tool's subcommand names; one counts only as the first argument.
/// * `classify` - Given an option name such as `--explain` or `-h`, returns its arity if it is
///   one of the tool's own options.
///
/// # Returns
///
/// The three groups, each in its original order.
pub fn split(
    args: impl IntoIterator<Item = OsString>,
    subcommands: &[&str],
    classify: impl Fn(&str) -> Option<Arity>,
) -> SplitArgs {
    // `peekable` lets the loop look at the next item without taking it.
    let mut args = args.into_iter().peekable();
    if args.peek().is_some_and(|arg| arg == SUBCOMMAND_NAME) {
        args.next();
    }
    let mut split = SplitArgs::default();
    if args
        .peek()
        .and_then(|arg| arg.to_str())
        .is_some_and(|arg| subcommands.contains(&arg))
    {
        split.own.extend(args.next());
    }
    while let Some(arg) = args.next() {
        if arg == "--" {
            // `by_ref` lends the iterator to `extend` instead of giving it away.
            split.test.extend(args.by_ref());
            break;
        }
        let text = arg.to_str();
        match text.and_then(option_name).and_then(&classify) {
            Some(arity) => {
                let needs_value = arity == Arity::Value && !text.is_some_and(|t| t.contains('='));
                split.own.push(arg);
                if needs_value {
                    split.own.extend(args.next());
                }
            }
            None => split.cargo.0.push(arg),
        }
    }
    split
}

/// The option name of an argument: `--name` from `--name=value`, or `None` if it is not an
/// option at all.
fn option_name(arg: &str) -> Option<&str> {
    if !arg.starts_with('-') || arg == "-" {
        return None;
    }
    Some(arg.split_once('=').map_or(arg, |(name, _)| name))
}

/// The value attached to `arg` for `flag`: `v` from `--flag=v`, or from `-pv` for a short flag.
fn attached_value<'a>(arg: &'a str, flag: &str) -> Option<&'a str> {
    let rest = arg.strip_prefix(flag)?;
    if let Some(value) = rest.strip_prefix('=') {
        return Some(value);
    }
    let is_short = flag.len() == 2;
    (is_short && !rest.is_empty()).then_some(rest)
}

/// Whether `arg` is `flag`, alone or with an attached value.
fn is_flag(arg: &OsStr, flag: &str) -> bool {
    arg.to_str()
        .is_some_and(|text| text == flag || attached_value(text, flag).is_some())
}

/// The arguments for `cargo test`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CargoArgs(Vec<OsString>);

impl CargoArgs {
    /// Wraps arguments.
    ///
    /// # Arguments
    ///
    /// * `args` - Arguments for `cargo test`, in order.
    ///
    /// # Returns
    ///
    /// The wrapper.
    pub fn new(args: Vec<OsString>) -> Self {
        Self(args)
    }

    /// The arguments, in order.
    pub fn as_slice(&self) -> &[OsString] {
        &self.0
    }

    /// Whether `flag` is given, alone or with a value.
    ///
    /// # Arguments
    ///
    /// * `flag` - Such as `--no-fail-fast`.
    ///
    /// # Returns
    ///
    /// `true` if any argument is that flag.
    pub fn has(&self, flag: &str) -> bool {
        self.0.iter().any(|arg| is_flag(arg, flag))
    }

    /// The value of the last occurrence of `flag`, as `--flag value` or `--flag=value`.
    ///
    /// # Arguments
    ///
    /// * `flag` - Such as `--target-dir`.
    ///
    /// # Returns
    ///
    /// The value, or `None` when the flag is absent or has no value.
    pub fn value(&self, flag: &str) -> Option<&OsStr> {
        let mut found = None;
        let mut iter = self.0.iter();
        while let Some(arg) = iter.next() {
            let Some(text) = arg.to_str() else { continue };
            if text == flag {
                found = iter.next().map(OsString::as_os_str);
            } else if let Some(value) = attached_value(text, flag) {
                found = Some(OsStr::new(value));
            }
        }
        found
    }

    /// The first argument that makes `cargo test` run unchanged (`--no-run`, `--doc`).
    pub fn passthrough_flag(&self) -> Option<&'static str> {
        PASSTHROUGH.into_iter().find(|flag| self.has(flag))
    }

    /// The first argument this tool refuses (`--message-format`).
    pub fn unsupported_flag(&self) -> Option<&'static str> {
        UNSUPPORTED.into_iter().find(|flag| self.has(flag))
    }

    /// Whether the arguments select targets, which turns doc tests off in `cargo test`.
    pub fn selects_targets(&self) -> bool {
        TARGET_SELECTION
            .into_iter()
            .chain(TARGET_SELECTION_WITH_VALUE)
            .any(|flag| self.has(flag))
    }

    /// The arguments without package or target selection, for running one target through
    /// `cargo test -p <package> --test <name>`.
    ///
    /// # Returns
    ///
    /// Every other argument, in order.
    #[must_use]
    pub fn without_selection(&self) -> Self {
        let with_value = || {
            PACKAGE_SELECTION_WITH_VALUE
                .into_iter()
                .chain(TARGET_SELECTION_WITH_VALUE)
        };
        let switches = || PACKAGE_SELECTION.into_iter().chain(TARGET_SELECTION);
        let mut kept = Vec::with_capacity(self.0.len());
        let mut iter = self.0.iter();
        while let Some(arg) = iter.next() {
            if let Some(text) = arg.to_str() {
                if with_value().any(|flag| text == flag) {
                    iter.next();
                    continue;
                }
                if with_value().any(|flag| attached_value(text, flag).is_some())
                    || switches().any(|flag| text == flag)
                {
                    continue;
                }
            }
            kept.push(arg.clone());
        }
        Self(kept)
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    use hegel::TestCase;
    use hegel::generators::{self as gs, Generator};

    /// Classifies the options this test pretends the tool has.
    fn classify(name: &str) -> Option<Arity> {
        match name {
            "--explain" | "--no-cache" => Some(Arity::Switch),
            "--log-format" => Some(Arity::Value),
            _ => None,
        }
    }

    /// A run of arguments that belong together, and the group they belong to.
    #[derive(Debug, Clone, Copy)]
    enum Piece {
        Own(&'static [&'static str]),
        Cargo(&'static [&'static str]),
    }

    const PIECES: [Piece; 7] = [
        Piece::Own(&["--explain"]),
        Piece::Own(&["--log-format", "json"]),
        Piece::Own(&["--log-format=text"]),
        Piece::Cargo(&["--release"]),
        Piece::Cargo(&["-p", "alpha"]),
        Piece::Cargo(&["--features=x"]),
        Piece::Cargo(&["--test", "it"]),
    ];

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    // Property: own options (with their values) go to `own`, everything else before `--` to
    // `cargo`, everything after the first `--` to `test`, each in its original order.
    #[hegel::test]
    fn split_sorts_every_argument_into_its_group(tc: TestCase) {
        let leading_subcommand_name = tc.draw(gs::booleans());
        let pieces = tc.draw(gs::vecs(gs::sampled_from(&PIECES).print_as_debug()).max_size(6));
        let test = tc.draw(
            gs::vecs(gs::sampled_from(&["--explain", "--", "filter", "-p"]).print_as_debug())
                .max_size(4),
        );
        let separator = tc.draw(gs::booleans()) || !test.is_empty();

        let mut argv = Vec::new();
        let (mut own, mut cargo) = (Vec::new(), Vec::new());
        if leading_subcommand_name {
            argv.push(SUBCOMMAND_NAME);
        }
        for piece in &pieces {
            match piece {
                Piece::Own(args) => {
                    argv.extend_from_slice(args);
                    own.extend_from_slice(args);
                }
                Piece::Cargo(args) => {
                    argv.extend_from_slice(args);
                    cargo.extend_from_slice(args);
                }
            }
        }
        if separator {
            argv.push("--");
            argv.extend_from_slice(&test);
        }

        let split = split(os(&argv), &["status"], classify);

        assert_eq!(split.own, os(&own));
        assert_eq!(split.cargo.as_slice(), os(&cargo).as_slice());
        assert_eq!(split.test, os(&test));
    }

    // Property: a subcommand name counts only as the first argument.
    #[hegel::test]
    fn subcommands_are_recognized_only_first(tc: TestCase) {
        let first = tc.draw(gs::booleans());

        let argv = if first {
            vec!["status", "-p", "x"]
        } else {
            vec!["-p", "status"]
        };
        let split = split(os(&argv), &["status"], classify);

        assert_eq!(
            split.own.first().map(OsString::as_os_str) == Some(OsStr::new("status")),
            first
        );
    }

    // Property: `value` finds the last occurrence, in either `--flag value` or `--flag=value`.
    #[hegel::test]
    fn value_returns_the_last_occurrence(tc: TestCase) {
        let occurrences = tc.draw(
            gs::vecs(gs::tuples!(
                gs::booleans(),
                gs::text().alphabet("abc/").min_size(1).max_size(6)
            ))
            .max_size(4),
        );
        let mut args = vec![OsString::from("--release")];
        for (attached, value) in &occurrences {
            if *attached {
                args.push(format!("--target-dir={value}").into());
            } else {
                args.push("--target-dir".into());
                args.push(value.into());
            }
        }

        let cargo = CargoArgs::new(args);

        let expected = occurrences.last().map(|(_, value)| OsString::from(value));
        assert_eq!(
            cargo.value("--target-dir").map(OsStr::to_os_string),
            expected
        );
    }

    const SELECTION: [&[&str]; 7] = [
        &["-p", "x"],
        &["-px"],
        &["--package=x"],
        &["--workspace"],
        &["--test", "t"],
        &["--lib"],
        &["--exclude", "y"],
    ];
    const OTHER: [&[&str]; 4] = [
        &["--release"],
        &["--features", "f"],
        &["-j", "4"],
        &["--locked"],
    ];

    // Property: `without_selection` drops exactly the package and target selection arguments,
    // and `selects_targets` sees a target selection exactly when there is one.
    #[hegel::test]
    fn without_selection_keeps_everything_else(tc: TestCase) {
        let pieces = tc.draw(
            gs::vecs(gs::tuples!(
                gs::booleans(),
                gs::integers::<usize>().max_value(6)
            ))
            .max_size(6),
        );
        let mut args = Vec::new();
        let mut kept = Vec::new();
        let mut targets = false;
        for (selection, index) in pieces {
            if selection {
                let piece = SELECTION[index];
                targets |= piece[0] == "--test" || piece[0] == "--lib";
                args.extend(os(piece));
            } else {
                let piece = OTHER[index % OTHER.len()];
                args.extend(os(piece));
                kept.extend(os(piece));
            }
        }
        let cargo = CargoArgs::new(args);

        assert_eq!(cargo.without_selection().as_slice(), kept.as_slice());
        assert_eq!(cargo.selects_targets(), targets);
    }
}
