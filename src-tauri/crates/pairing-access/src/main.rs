fn main() {
    std::process::exit(xshell_pairing_access::cli_main(
        &std::env::args_os().skip(1).collect::<Vec<_>>(),
    ));
}
