fn main() {
    // The build identity (Lesani/xshell#49): `XSHELL_BUILDINFO_STAMP`, see `src/build_id.rs`.
    xshell_buildinfo::emit();
}
