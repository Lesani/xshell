#[cfg(unix)]
fn main() {
    std::process::exit(xshelld::main_entry());
}

#[cfg(not(unix))]
fn main() {
    eprintln!("xshelld supports Unix hosts only");
    std::process::exit(1);
}
