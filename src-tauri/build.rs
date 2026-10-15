fn main() {
    // The build identity (Lesani/xshell#49): `XSHELL_BUILDINFO_STAMP`, see `src/build_id.rs`.
    xshell_buildinfo::emit();
    // The GitHub repo whose releases provide xshelld binaries (forks set their own).
    println!("cargo:rerun-if-env-changed=XSHELL_RELEASE_REPO");
    tauri_build::build()
}
