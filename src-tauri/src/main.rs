#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // `xshell event …`: an agent hook in a Local Host Terminal, handled before any window.
    if let Some(code) = xshell_lib::run_cli() {
        std::process::exit(code);
    }
    xshell_lib::run()
}
