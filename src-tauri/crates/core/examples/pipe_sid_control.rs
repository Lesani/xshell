//! Owned native fixture helper. It never reads profiles or changes privileges.
#[cfg(windows)]
fn main() {
    use std::path::Path;
    use std::time::{Duration, Instant};
    use xshell_core::pipe::{connect, current_user_sid, user_only_sddl, PipeListener};
    let args: Vec<String> = std::env::args().collect();
    assert_eq!(args.len(), 5, "mode endpoint ready-file release-file");
    let endpoint = Path::new(&args[2]);
    let ready = Path::new(&args[3]);
    let release = Path::new(&args[4]);
    match args[1].as_str() {
        "serve-private" | "serve-permissive" => {
            let sid = current_user_sid().unwrap().to_string_sid().unwrap();
            let sddl = if args[1] == "serve-private" {
                user_only_sddl(&sid)
            } else {
                format!("O:{sid}D:P(A;;GA;;;WD)")
            };
            let _listener = PipeListener::bind_with_sddl(endpoint, &sddl).unwrap();
            std::fs::write(ready, &sid).unwrap();
            let until = Instant::now() + Duration::from_secs(90);
            while !release.exists() {
                assert!(Instant::now() < until, "owned SID helper deadline");
                std::thread::sleep(Duration::from_millis(20));
            }
            println!("SID-SERVER-CLOSED {sid}");
        }
        "connect-denied" | "connect-owner-refused" => {
            let owner = std::fs::read_to_string(ready).unwrap();
            let sid = current_user_sid().unwrap().to_string_sid().unwrap();
            assert_ne!(sid, owner, "cross-SID control requires two actual users");
            let e =
                connect(endpoint, Instant::now() + Duration::from_secs(2), &|| false).unwrap_err();
            assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied, "{e}");
            if args[1] == "connect-owner-refused" {
                assert!(
                    e.to_string().contains("not owned by this user"),
                    "must reach the client owner check, not merely DACL denial: {e}"
                );
            } else {
                assert_eq!(
                    e.raw_os_error(),
                    Some(5),
                    "must be Windows access denial before dispatch: {e}"
                );
            }
            println!("CROSS-SID-PASS {} client={sid} owner={owner}", args[1]);
        }
        _ => panic!("unknown SID helper mode"),
    }
}
#[cfg(not(windows))]
fn main() {
    panic!("native Windows SID helper requires Windows");
}
