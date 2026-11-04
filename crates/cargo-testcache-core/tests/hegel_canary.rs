//! A canary proving the Hegel engine actually generates cases.
//!
//! On success, `#[hegel::test]` prints the same `... ok` as a plain `#[test]`, whether it
//! explored a hundred inputs or none. This test is the one that says how many cases it ran:
//! at `Verbose` it prints one `Running test case` line per case, and `just hegel-canary` fails
//! if that count drops below its floor.
//!
//! It is its own integration target so its noisy output stays out of the library's tests.

use hegel::generators;

/// Draws one value per case and asserts nothing: it checks the test runner, not the crate.
///
/// `test_cases` is pinned so the recipe's floor means something; the count lands a little
/// under it because the run opens by replaying earlier failures, of which there are none.
#[hegel::test(test_cases = 100, verbosity = hegel::Verbosity::Verbose)]
fn hegel_generates_cases(tc: hegel::TestCase) {
    let _ = tc.draw(generators::integers::<u8>());
}
