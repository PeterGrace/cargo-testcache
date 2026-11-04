//! Test support for the fixture workspace: lets each test report that it ran, dump its
//! environment, or fail on demand, through variables the cache does not key on.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

/// Call first in every fixture test.
///
/// - `FIXTURE_RUN_LOG`: appends this binary's name and a newline.
/// - `FIXTURE_DUMP_DIR`: writes `<name>.env`, the working directory and the environment.
/// - `FIXTURE_FAIL_DIR`: panics if a file named after this binary exists there.
pub fn record() {
    let name = binary_name();
    if let Some(log) = std::env::var_os("FIXTURE_RUN_LOG") {
        let mut file =
            OpenOptions::new().create(true).append(true).open(log).expect("open the run log");
        writeln!(file, "{name}").expect("append to the run log");
    }
    if let Some(dir) = std::env::var_os("FIXTURE_DUMP_DIR") {
        let mut vars: Vec<String> = std::env::vars_os()
            .map(|(key, value)| format!("{}={}", key.to_string_lossy(), value.to_string_lossy()))
            .collect();
        vars.sort();
        let cwd = std::env::current_dir().expect("working directory");
        let dump = format!("cwd={}\n{}\n", cwd.display(), vars.join("\n"));
        fs::write(PathBuf::from(dir).join(format!("{name}.env")), dump).expect("write the dump");
    }
    if let Some(dir) = std::env::var_os("FIXTURE_FAIL_DIR") {
        assert!(!PathBuf::from(dir).join(&name).exists(), "{name} was told to fail");
    }
}

/// The test binary's name without Cargo's hash: `alpha` for `alpha-0123abcd`.
fn binary_name() -> String {
    let exe = std::env::current_exe().expect("current executable");
    let stem = exe.file_stem().expect("a file name").to_string_lossy().into_owned();
    stem.rsplit_once('-').map_or_else(|| stem.clone(), |(name, _)| name.to_owned())
}
