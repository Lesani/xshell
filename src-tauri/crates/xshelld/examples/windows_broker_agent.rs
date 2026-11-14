//! Native fixture launcher for the raw broker Terminal. Never runs a provider executable.
#[cfg(windows)]
fn main() {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let exe = std::env::current_exe().unwrap();
    let dir = exe.parent().unwrap();
    let script = dir.join("agent.cmd");
    assert!(script.is_file(), "owned .cmd fixture required");
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("launches.log"))
        .unwrap();
    writeln!(log, "launch").unwrap();
    log.sync_all().unwrap();
    let status = Command::new("cmd.exe")
        .args(["/D", "/Q", "/K"])
        .arg(script)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .unwrap();
    std::process::exit(status.code().unwrap_or(1));
}
#[cfg(not(windows))]
fn main() {
    panic!("native Windows broker fixture requires Windows");
}
