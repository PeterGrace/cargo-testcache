//! Spawns the package's binary through `CARGO_BIN_EXE_betatool`.

#[test]
fn spawns_tool() {
    probe::record();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_betatool"))
        .output()
        .expect("run betatool");
    assert!(output.status.success());
}
