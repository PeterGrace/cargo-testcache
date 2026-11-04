//! Fixture library `alpha`.

/// Changed by the tests to rebuild `alpha` and everything that depends on it.
pub const STAMP: &str = "0";

/// Returns [`STAMP`].
///
/// ```
/// assert!(!alpha::stamp().is_empty());
/// ```
pub fn stamp() -> &'static str {
    STAMP
}

#[cfg(test)]
mod tests {
    #[test]
    fn alpha_unit() {
        probe::record();
        println!("{}", super::stamp());
    }
}
