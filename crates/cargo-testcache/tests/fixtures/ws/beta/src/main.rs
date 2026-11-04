//! Fixture binary `betatool`.

/// Changed by the tests to rebuild `betatool` alone.
const STAMP: &str = "0";

fn main() {
    println!("betatool {STAMP}");
}

#[cfg(test)]
mod tests {
    #[test]
    fn betatool_unit() {
        probe::record();
        println!("{}", super::STAMP);
    }
}
