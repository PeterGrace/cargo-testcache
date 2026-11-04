//! Runtime configuration, read from `CARGO_TESTCACHE_*` environment variables.
//!
//! Settings layer from lowest to highest precedence: built-in defaults, environment variables
//! (a `.env` file is loaded into the environment first), then command-line overrides applied
//! by the binary with the `with_*` methods. `RUST_LOG` is not read here: the binary hands it
//! straight to the log filter.
//!
//! [`Config::from_lookup`] takes the environment as a function, so tests pass a map instead of
//! setting process environment variables, which is `unsafe` in edition 2024 because other
//! threads may be reading the environment at the same time.

use std::ffi::OsString;
use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use crate::error::{Error, Result};

/// Environment variable naming the log format, `text` or `json`.
pub const LOG_FORMAT_VAR: &str = "CARGO_TESTCACHE_LOG_FORMAT";

/// Environment variable giving the longest wait, in whole seconds, for the application to stop
/// after a shutdown signal.
pub const SHUTDOWN_TIMEOUT_VAR: &str = "CARGO_TESTCACHE_SHUTDOWN_TIMEOUT_SECS";

/// The default for [`SHUTDOWN_TIMEOUT_VAR`]. Kubernetes waits 30 seconds between SIGTERM and
/// SIGKILL, so the default stays below it.
pub const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(25);

/// How log lines are written to stderr.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogFormat {
    /// Human-readable lines, the default.
    #[default]
    Text,
    /// One JSON object per line, for log collectors.
    Json,
}

// `FromStr` is what `"json".parse::<LogFormat>()` calls, and what clap uses to parse
// `--log-format json`.
impl FromStr for LogFormat {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        if value.eq_ignore_ascii_case("text") {
            Ok(Self::Text)
        } else if value.eq_ignore_ascii_case("json") {
            Ok(Self::Json)
        } else {
            Err(Error::UnknownLogFormat {
                value: value.to_owned(),
            })
        }
    }
}

// `Display` is Rust's `__str__`; it is also how clap shows a default value.
impl fmt::Display for LogFormat {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Text => "text",
            Self::Json => "json",
        })
    }
}

/// The application's settings.
///
/// Fields are private; read them through the accessors, and build a value with
/// [`Config::from_env`], [`Config::from_lookup`] or [`Config::default`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    log_format: LogFormat,
    shutdown_timeout: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            log_format: LogFormat::default(),
            shutdown_timeout: DEFAULT_SHUTDOWN_TIMEOUT,
        }
    }
}

impl Config {
    /// Reads the configuration from the process environment.
    ///
    /// # Returns
    ///
    /// The configuration, with defaults for every variable that is unset.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidConfig`], listing every variable that is set but invalid.
    pub fn from_env() -> Result<Self> {
        Self::from_lookup(|name| std::env::var_os(name))
    }

    /// Reads the configuration through `lookup`, which returns a variable's value, or `None`
    /// when it is unset.
    ///
    /// # Arguments
    ///
    /// * `lookup` - Called once per variable name, like `os.environ.get` in Python.
    ///
    /// # Returns
    ///
    /// The configuration, with defaults for every variable that is unset.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidConfig`], listing every invalid variable rather than only the first.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use cargo_testcache_core::{Config, LogFormat};
    ///
    /// let config = Config::from_lookup(|name| match name {
    ///     "CARGO_TESTCACHE_LOG_FORMAT" => Some("json".into()),
    ///     _ => None,
    /// })
    /// .expect("valid configuration");
    /// assert_eq!(config.log_format(), LogFormat::Json);
    /// assert_eq!(config.shutdown_timeout(), Duration::from_secs(25));
    /// ```
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<OsString>) -> Result<Self> {
        let defaults = Self::default();
        let mut problems = Vec::new();

        let log_format = parse_var(&lookup, LOG_FORMAT_VAR, &mut problems);
        let shutdown_secs = parse_var::<u64>(&lookup, SHUTDOWN_TIMEOUT_VAR, &mut problems);

        if !problems.is_empty() {
            return Err(Error::InvalidConfig { problems });
        }
        Ok(Self {
            log_format: log_format.unwrap_or(defaults.log_format),
            shutdown_timeout: shutdown_secs.map_or(defaults.shutdown_timeout, Duration::from_secs),
        })
    }

    /// Replaces the log format, for a command-line override.
    ///
    /// # Arguments
    ///
    /// * `log_format` - The new format.
    ///
    /// # Returns
    ///
    /// The updated configuration.
    #[must_use]
    pub fn with_log_format(mut self, log_format: LogFormat) -> Self {
        self.log_format = log_format;
        self
    }

    /// How log lines are written.
    pub fn log_format(&self) -> LogFormat {
        self.log_format
    }

    /// The longest wait for the application to stop after a shutdown signal.
    pub fn shutdown_timeout(&self) -> Duration {
        self.shutdown_timeout
    }
}

/// Reads and parses one variable through `lookup`.
///
/// # Arguments
///
/// * `lookup` - The environment, as in [`Config::from_lookup`].
/// * `name` - The variable to read.
/// * `problems` - Where a message is appended if the variable is set but invalid.
///
/// # Returns
///
/// The parsed value, or `None` if the variable is unset or invalid.
fn parse_var<T>(
    lookup: &impl Fn(&str) -> Option<OsString>,
    name: &str,
    problems: &mut Vec<String>,
) -> Option<T>
where
    T: FromStr,
    T::Err: fmt::Display,
{
    let raw = lookup(name)?;
    // Environment values are OS strings, which need not be valid UTF-8.
    let Some(text) = raw.to_str() else {
        problems.push(format!("{name} is not valid UTF-8"));
        return None;
    };
    text.parse()
        .map_err(|error| problems.push(format!("{name}={text:?}: {error}")))
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashMap;

    use hegel::TestCase;
    use hegel::generators::{self as gs, Generator};

    /// Builds a `lookup` over `vars`, standing in for the process environment.
    fn lookup_in<'a>(vars: &'a HashMap<&str, String>) -> impl Fn(&str) -> Option<OsString> + 'a {
        |name| vars.get(name).map(OsString::from)
    }

    // Property: a format's name parses back to the same format, in any letter case.
    #[hegel::test]
    fn log_format_names_round_trip(tc: TestCase) {
        let format =
            tc.draw(gs::sampled_from(&[LogFormat::Text, LogFormat::Json]).print_as_debug());
        let uppercase = tc.draw(gs::booleans());
        let name = format.to_string();
        let name = if uppercase { name.to_uppercase() } else { name };

        let parsed: LogFormat = name.parse().expect("a format's own name parses");

        assert_eq!(parsed, format);
    }

    // Property: each variable that is set is used, and each one that is unset gets its default.
    #[hegel::test]
    fn valid_values_are_read_and_unset_ones_default(tc: TestCase) {
        let format = tc.draw(gs::optional(
            gs::sampled_from(&[LogFormat::Text, LogFormat::Json]).print_as_debug(),
        ));
        let timeout_secs = tc.draw(gs::optional(gs::integers::<u64>()));
        let mut vars = HashMap::new();
        if let Some(format) = format {
            vars.insert(LOG_FORMAT_VAR, format.to_string());
        }
        if let Some(secs) = timeout_secs {
            vars.insert(SHUTDOWN_TIMEOUT_VAR, secs.to_string());
        }

        let config = Config::from_lookup(lookup_in(&vars)).expect("valid values are accepted");

        assert_eq!(config.log_format(), format.unwrap_or_default());
        assert_eq!(
            config.shutdown_timeout(),
            timeout_secs.map_or(DEFAULT_SHUTDOWN_TIMEOUT, Duration::from_secs)
        );
    }

    // Property: every invalid variable is reported, not only the first one found.
    #[hegel::test]
    fn every_invalid_value_is_reported(tc: TestCase) {
        let format = tc.draw(gs::text().filter(|text: &String| text.parse::<LogFormat>().is_err()));
        let timeout = tc.draw(gs::text().filter(|text: &String| text.parse::<u64>().is_err()));
        let vars = HashMap::from([(LOG_FORMAT_VAR, format), (SHUTDOWN_TIMEOUT_VAR, timeout)]);

        let error = Config::from_lookup(lookup_in(&vars)).expect_err("invalid values are refused");

        let Error::InvalidConfig { problems } = error else {
            panic!("expected InvalidConfig, got {error:?}");
        };
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems[0].starts_with(LOG_FORMAT_VAR), "{problems:?}");
        assert!(
            problems[1].starts_with(SHUTDOWN_TIMEOUT_VAR),
            "{problems:?}"
        );
    }

    // Property: a command-line override wins over whatever the environment set.
    #[hegel::test]
    fn with_log_format_overrides_the_environment(tc: TestCase) {
        let formats = [LogFormat::Text, LogFormat::Json];
        let from_env = tc.draw(gs::sampled_from(&formats).print_as_debug());
        let from_cli = tc.draw(gs::sampled_from(&formats).print_as_debug());
        let vars = HashMap::from([(LOG_FORMAT_VAR, from_env.to_string())]);

        let config = Config::from_lookup(lookup_in(&vars))
            .expect("valid values are accepted")
            .with_log_format(from_cli);

        assert_eq!(config.log_format(), from_cli);
    }
}
