fn main() {
    // The GitHub repo whose releases provide xshelld binaries (forks set their own).
    println!("cargo:rerun-if-env-changed=XSHELL_RELEASE_REPO");
    tauri_build::build()
}
