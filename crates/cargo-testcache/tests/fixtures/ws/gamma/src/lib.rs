//! Fixture library `gamma`, which depends on `alpha`.

/// Changed by the tests to rebuild `gamma` alone. The unit test prints it, so a change reaches
/// the test executable.
pub const STAMP: &str = "0";

/// `alpha`'s stamp, twice.
pub fn twice() -> String {
    format!("{0}{0}", alpha::stamp())
}

#[cfg(test)]
mod tests {
    #[test]
    fn gamma_unit() {
        probe::record();
        println!("{} {}", super::twice(), super::STAMP);
    }
}
