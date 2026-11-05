//! Reads a workspace input, a package input and this target's own input, relative to the
//! package root.

#[test]
fn reads_input() {
    probe::record();
    let value = std::fs::read_to_string("../data/value.txt").expect("read data/value.txt");
    let extra = std::fs::read_to_string("fixtures/extra.txt").expect("read fixtures/extra.txt");
    let own = std::fs::read_to_string("reads_input_data/value.txt")
        .expect("read reads_input_data/value.txt");
    println!("{} {value} {extra} {own}", alpha::stamp());
}
