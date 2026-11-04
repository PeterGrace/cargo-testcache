//! Reads a workspace input and a package input, relative to the package root.

#[test]
fn reads_input() {
    probe::record();
    let value = std::fs::read_to_string("../data/value.txt").expect("read data/value.txt");
    let extra = std::fs::read_to_string("fixtures/extra.txt").expect("read fixtures/extra.txt");
    println!("{} {value} {extra}", alpha::stamp());
}
