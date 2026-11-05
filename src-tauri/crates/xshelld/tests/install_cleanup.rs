#![cfg(target_os = "linux")]
//! Real maintenance CLI behavior, including removal of existing, unmarked installs.
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Fixture {
    temp: tempfile::TempDir,
    uid: u32,
    home: PathBuf,
    root: PathBuf,
    current: PathBuf,
    old: String,
}

impl Fixture {
    fn new() -> Self {
        assert!(
            Command::new("sudo")
                .args(["-n", "true"])
                .status()
                .unwrap()
                .success(),
            "these positive fixtures need sudo solely for isolated numeric fixture ownership"
        );
        let temp = tempfile::tempdir().unwrap();
        let uid = 100_000 + (uuid::Uuid::new_v4().as_u128() % 900_000_000) as u32;
        // Never share a UID with an existing process.
        for entry in fs::read_dir("/proc").unwrap().flatten() {
            if entry
                .file_name()
                .to_str()
                .and_then(|s| s.parse::<u32>().ok())
                .is_none()
            {
                continue;
            }
            match fs::read_to_string(entry.path().join("status")) {
                Ok(status) => {
                    let ids = status.lines().find_map(|s| s.strip_prefix("Uid:")).unwrap();
                    assert!(!ids
                        .split_whitespace()
                        .any(|id| id.parse::<u32>().ok() == Some(uid)));
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => panic!("cannot check fixture UID availability: {e}"),
            }
        }
        let home = temp.path().join("a");
        let root = home.join(".xshell/server");
        fs::create_dir_all(&root).unwrap();
        let build = xshell_protocol::build::parse_stamp(env!("XSHELL_BUILDINFO_STAMP"))
            .unwrap()
            .unwrap();
        let name =
            xshell_protocol::build::install_dir_name(env!("CARGO_PKG_VERSION"), Some(&build));
        let current = root.join(name).join("xshelld");
        fs::create_dir_all(current.parent().unwrap()).unwrap();
        fs::copy(env!("CARGO_BIN_EXE_xshelld"), &current).unwrap();
        fs::set_permissions(&current, fs::Permissions::from_mode(0o700)).unwrap();
        let old = "1.0.0+111111111111".to_string();
        install(&root, &old, "111111111111");
        for path in [
            temp.path(),
            &home,
            &home.join(".xshell"),
            &root,
            current.parent().unwrap(),
            &root.join(&old),
        ] {
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        Self {
            temp,
            uid,
            home,
            root,
            current,
            old,
        }
    }

    fn own(&self) {
        assert!(Command::new("sudo")
            .args(["-n", "chown", "-R", &format!("{}:{}", self.uid, self.uid)])
            .arg(self.temp.path())
            .status()
            .unwrap()
            .success());
    }

    fn cli(&self, args: &[&str]) -> Output {
        self.own();
        self.cli_owned(args)
    }

    fn cli_owned(&self, args: &[&str]) -> Output {
        let out = Command::new("sudo").args(["-n", "python3", "-c", "import os,sys; u=int(sys.argv[1]); os.setgroups([]); os.setgid(u); os.setuid(u); os.execv(sys.argv[2],sys.argv[2:])", &self.uid.to_string()])
            .arg(&self.current).args(["maintenance", "cleanup-installs", "--home"])
            .arg(&self.home).args(args)
            .env_remove("XSHELL_EVENT_SOCKET").env_remove("XSHELL_TERMINAL_ID")
            .output().unwrap();
        self.restore();
        out
    }

    fn restore(&self) {
        assert!(Command::new("sudo")
            .args([
                "-n",
                "chown",
                "-R",
                &format!("{}:{}", unsafe { libc::getuid() }, unsafe {
                    libc::getgid()
                })
            ])
            .arg(self.temp.path())
            .status()
            .unwrap()
            .success());
    }
}

// sudo-owned fixtures are returned to this test's UID before TempDir removes them.
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = Command::new("sudo")
            .args([
                "-n",
                "chown",
                "-R",
                &format!("{}:{}", unsafe { libc::getuid() }, unsafe {
                    libc::getgid()
                }),
            ])
            .arg(self.temp.path())
            .status();
    }
}

fn install(root: &Path, name: &str, stamp: &str) {
    let dir = root.join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    let file = dir.join("xshelld");
    fs::write(
        &file,
        format!("#!/bin/sh\n# @@XSHELL-BUILD-V1:{stamp}:1@@\nexit 0\n"),
    )
    .unwrap();
    fs::set_permissions(file, fs::Permissions::from_mode(0o700)).unwrap();
}

fn success(out: &Output) {
    assert!(
        out.status.success(),
        "status={} stdout={} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
#[ignore = "requires sudo only for owned numeric-UID fixtures; run explicitly with --ignored"]
fn dry_run_then_delete_existing_keyed() {
    let f = Fixture::new();
    let selected = "0.9.0+222222222222";
    let hook = "1.0.0+333333333333";
    install(&f.root, selected, "222222222222");
    install(&f.root, hook, "333333333333");
    install(&f.root, "0.8.0", "444444444444");
    install(&f.root, "1.0.0+555555555555", "555555555555");
    fs::write(
        f.root.join("1.0.0+555555555555/unexpected"),
        b"keep ambiguous",
    )
    .unwrap();
    fs::write(f.root.join("selected"), format!("{selected}\n")).unwrap();
    fs::set_permissions(f.root.join("selected"), fs::Permissions::from_mode(0o600)).unwrap();
    let daemon = f.home.join(".xshell/daemon");
    fs::create_dir(&daemon).unwrap();
    fs::set_permissions(&daemon, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(
        daemon.join("claude-hooks.json"),
        xshell_core::agent_status::claude_settings_json(&f.root.join(hook).join("xshelld")),
    )
    .unwrap();
    fs::set_permissions(
        daemon.join("claude-hooks.json"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let outside = f.temp.path().join("outside-sentinel");
    fs::write(&outside, b"outside").unwrap();
    let survivors = [
        f.current.clone(),
        f.root.join(selected).join("xshelld"),
        f.root.join(hook).join("xshelld"),
        f.root.join("0.8.0/xshelld"),
        f.root.join("1.0.0+555555555555/xshelld"),
        f.root.join("selected"),
        outside,
    ];
    let before: Vec<_> = survivors
        .iter()
        .map(|p| (fs::read(p).unwrap(), fs::metadata(p).unwrap().ino()))
        .collect();
    let old_file = f.root.join(&f.old).join("xshelld");
    let bytes = fs::read(&old_file).unwrap();
    let inode = fs::metadata(&old_file).unwrap().ino();
    let out = f.cli(&[]);
    success(&out);
    assert!(String::from_utf8_lossy(&out.stdout).contains("would-remove"));
    assert_eq!(fs::read(&old_file).unwrap(), bytes);
    assert_eq!(fs::metadata(&old_file).unwrap().ino(), inode);
    success(&f.cli(&["--delete", "--confirm-quiescent"]));
    assert!(
        !f.root.join(&f.old).exists(),
        "must remove executable AND install directory"
    );
    assert!(f.current.exists());
    for (path, (bytes, inode)) in survivors.iter().zip(before) {
        assert_eq!(fs::read(path).unwrap(), bytes, "survivor bytes {path:?}");
        assert_eq!(
            fs::metadata(path).unwrap().ino(),
            inode,
            "survivor inode {path:?}"
        );
    }
    assert!(Command::new(f.root.join(hook).join("xshelld"))
        .status()
        .unwrap()
        .success());
}

#[test]
#[ignore = "requires sudo only for owned numeric-UID fixtures; run explicitly with --ignored"]
fn delete_requires_explicit_contract() {
    let f = Fixture::new();
    for args in [
        vec!["--delete"],
        vec!["--confirm-quiescent"],
        vec!["--delete", "--confirm-quiescent", "--delete"],
    ] {
        assert_eq!(f.cli(&args).status.code(), Some(2));
        assert!(f.root.join(&f.old).join("xshelld").exists());
    }
}

#[test]
#[ignore = "requires sudo only for owned numeric-UID fixtures; run explicitly with --ignored"]
fn cross_home_hook_survives_and_custom_settings_block() {
    let f = Fixture::new();
    let b = f.temp.path().join("b");
    let daemon = b.join(".xshell/daemon");
    fs::create_dir_all(&daemon).unwrap();
    for path in [&b, &b.join(".xshell"), &daemon] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let hook = "1.0.0+333333333333";
    install(&f.root, hook, "333333333333");
    let settings = daemon.join("claude-hooks.json");
    // Production AgentHooks writer, with served HOME B and executable owned by A.
    let hooks = xshell_core::agent_status::AgentHooks {
        exe: f.root.join(hook).join("xshelld"),
        endpoint: "/unused-fixture.sock".into(),
        claude_settings: settings.clone(),
        pi_extension: None,
    };
    hooks.write_claude_settings().unwrap();
    let hook_before = fs::read(&hooks.exe).unwrap();
    let out = f.cli(&[
        "--reference-home",
        b.to_str().unwrap(),
        "--delete",
        "--confirm-quiescent",
    ]);
    success(&out);
    assert!(!f.root.join(&f.old).exists());
    assert_eq!(fs::read(&hooks.exe).unwrap(), hook_before);
    assert!(Command::new(&hooks.exe).status().unwrap().success());
    for contents in [
        b"{}".as_slice(),
        b"{invalid",
        b"{\"hooks\":{\"Stop\":[{\"hooks\":[{\"command\":\"custom-wrapper\"}]}]}}",
    ] {
        install(&f.root, &f.old, "111111111111");
        fs::write(&settings, contents).unwrap();
        assert_eq!(
            f.cli(&[
                "--reference-home",
                b.to_str().unwrap(),
                "--delete",
                "--confirm-quiescent"
            ])
            .status
            .code(),
            Some(1)
        );
        assert!(f.root.join(&f.old).join("xshelld").exists());
    }
    hooks.write_claude_settings().unwrap();
    fs::set_permissions(&settings, fs::Permissions::from_mode(0o000)).unwrap();
    assert_eq!(
        f.cli(&[
            "--reference-home",
            b.to_str().unwrap(),
            "--delete",
            "--confirm-quiescent"
        ])
        .status
        .code(),
        Some(1)
    );
    fs::set_permissions(&settings, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(f.root.join(&f.old).join("xshelld").exists());
}

#[test]
#[ignore = "requires sudo only for owned numeric-UID fixtures; run explicitly with --ignored"]
fn selected_and_layout_uncertainty() {
    use std::os::unix::fs::symlink;
    let f = Fixture::new();
    let selected = f.root.join("selected");
    for bytes in [
        b"bad/target\n".as_slice(),
        b"".as_slice(),
        b"1.0.0\nsecond\n".as_slice(),
    ] {
        fs::write(&selected, bytes).unwrap();
        assert_eq!(
            f.cli(&["--delete", "--confirm-quiescent"]).status.code(),
            Some(1)
        );
        assert!(f.root.join(&f.old).join("xshelld").exists());
    }
    fs::write(&selected, b"1.0.0\n").unwrap();
    fs::set_permissions(&selected, fs::Permissions::from_mode(0o000)).unwrap();
    assert_eq!(
        f.cli(&["--delete", "--confirm-quiescent"]).status.code(),
        Some(1)
    );
    fs::set_permissions(&selected, fs::Permissions::from_mode(0o600)).unwrap();
    fs::remove_file(&selected).unwrap();
    let outside = f.temp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("sentinel"), b"outside").unwrap();
    symlink(outside.join("sentinel"), &selected).unwrap();
    assert_eq!(
        f.cli(&["--delete", "--confirm-quiescent"]).status.code(),
        Some(1)
    );
    fs::remove_file(&selected).unwrap();
    symlink(&outside, f.root.join("1.0.0+aaaaaaaaaaaa")).unwrap();
    install(&f.root, "1.0.0+bbbbbbbbbbbb", "bbbbbbbbbbbb");
    fs::hard_link(
        f.root.join("1.0.0+bbbbbbbbbbbb/xshelld"),
        outside.join("hardlink"),
    )
    .unwrap();
    install(&f.root, "1.0.0+cccccccccccc", "cccccccccccc");
    fs::create_dir(f.root.join("1.0.0+cccccccccccc/nested")).unwrap();
    install(&f.root, "1.0.0+dddddddddddd", "eeeeeeeeeeee");
    install(&f.root, "1.0.0+ffffffffffff", "ffffffffffff");
    let out = f.cli(&[
        "--delete",
        "--confirm-quiescent",
        "--keep",
        "1.0.0+ffffffffffff",
    ]);
    success(&out);
    assert!(!f.root.join(&f.old).exists());
    for name in [
        "1.0.0+aaaaaaaaaaaa",
        "1.0.0+bbbbbbbbbbbb",
        "1.0.0+cccccccccccc",
        "1.0.0+dddddddddddd",
        "1.0.0+ffffffffffff",
    ] {
        assert!(f.root.join(name).exists());
    }
    assert_eq!(fs::read(outside.join("sentinel")).unwrap(), b"outside");
    // A symlink in a managed ancestor blocks globally, without touching its target.
    fs::rename(f.home.join(".xshell"), f.home.join("real-xshell")).unwrap();
    symlink(f.home.join("real-xshell"), f.home.join(".xshell")).unwrap();
    assert_eq!(
        f.cli(&["--delete", "--confirm-quiescent"]).status.code(),
        Some(1)
    );
    assert_eq!(fs::read(outside.join("sentinel")).unwrap(), b"outside");
}

struct FixtureProcess {
    child: std::process::Child,
    pid: u32,
    uid: u32,
}
impl Drop for FixtureProcess {
    fn drop(&mut self) {
        let _ = Command::new("sudo").args(["-n", "python3", "-c",
            "import os,signal,sys; p=int(sys.argv[1]); u=int(sys.argv[2]); f=os.pidfd_open(p); s=open('/proc/%d/status'%p).read(); ids=next(x[4:] for x in s.splitlines() if x.startswith('Uid:')); assert all(int(x)==u for x in ids.split()); signal.pidfd_send_signal(f,signal.SIGTERM)",
            &self.pid.to_string(), &self.uid.to_string()]).status();
        let _ = self.child.wait();
    }
}
impl Fixture {
    fn spawn(&self, program: &Path, args: &[&str]) -> FixtureProcess {
        use std::io::BufRead;
        let expected_executable = fs::canonicalize(program).unwrap();
        self.own();
        let mut child = Command::new("sudo")
            .args(["-n", "python3", "-c", "import os,sys; u=int(sys.argv[1]); os.setgroups([]); os.setgid(u); os.setuid(u); print(os.getpid(),flush=True); os.execv(sys.argv[2],sys.argv[2:])", &self.uid.to_string()])
            .arg(program).args(args).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null())
            .env_remove("XSHELL_EVENT_SOCKET").env_remove("XSHELL_TERMINAL_ID").spawn().unwrap();
        let mut line = String::new();
        std::io::BufReader::new(child.stdout.as_mut().unwrap())
            .read_line(&mut line)
            .unwrap();
        let pid: u32 = line.trim().parse().unwrap();
        let process = FixtureProcess {
            child,
            pid,
            uid: self.uid,
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let out = Command::new("sudo")
                .args(["-n", "python3", "-c", "import os,sys; u=int(sys.argv[1]); os.setgroups([]); os.setgid(u); os.setuid(u); print(os.readlink('/proc/%s/exe'%sys.argv[2]))", &self.uid.to_string(), &pid.to_string()])
                .output()
                .unwrap();
            if out.status.success()
                && Path::new(String::from_utf8_lossy(&out.stdout).trim()) == expected_executable
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "fixture did not exec expected program"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        process
    }
}

#[test]
#[ignore = "requires sudo only for owned numeric-UID fixtures; run explicitly with --ignored"]
fn active_and_unknown_processes_block() {
    let f = Fixture::new();
    let b = f.temp.path().join("b");
    fs::create_dir(&b).unwrap();
    fs::set_permissions(&b, fs::Permissions::from_mode(0o700)).unwrap();
    let socket = f.temp.path().join("custom.sock");
    let daemon = f.spawn(
        &f.current,
        &[
            "serve",
            "--home",
            b.to_str().unwrap(),
            "--socket",
            socket.to_str().unwrap(),
        ],
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !Command::new("sudo")
        .args(["-n", "test", "-S"])
        .arg(&socket)
        .status()
        .unwrap()
        .success()
    {
        assert!(
            std::time::Instant::now() < deadline,
            "fixture Daemon failed to start"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let out = f.cli(&["--delete", "--confirm-quiescent"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("observed Daemon"));
    assert!(f.root.join(&f.old).join("xshelld").exists());
    // A real connector with stdin held open, using the already-running isolated Daemon.
    let connector = f.spawn(
        &f.current,
        &[
            "connect",
            "--home",
            b.to_str().unwrap(),
            "--socket",
            socket.to_str().unwrap(),
        ],
    );
    assert_eq!(
        f.cli(&["--delete", "--confirm-quiescent"]).status.code(),
        Some(1)
    );
    drop(connector);
    drop(daemon);
    // An unrelated shell is not whitelisted by executable or process name.
    let shell = f.spawn(Path::new("/bin/sh"), &["-c", "read fixture_input"]);
    let out = f.cli(&["--delete", "--confirm-quiescent"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("unknown same-UID process"),
        "{out:?}"
    );
    assert!(f.root.join(&f.old).join("xshelld").exists());
    drop(shell);
    success(&f.cli(&["--delete", "--confirm-quiescent"]));
    assert!(!f.root.join(&f.old).exists());
}

#[test]
#[ignore = "requires sudo only for owned numeric-UID fixtures; run explicitly with --ignored"]
fn foreign_ownership_never_overridden() {
    let f = Fixture::new();
    f.own();
    assert!(Command::new("sudo")
        .args(["-n", "chown", "-R", &format!("{}:{}", f.uid + 1, f.uid + 1)])
        .arg(f.root.join(&f.old))
        .status()
        .unwrap()
        .success());
    success(&f.cli_owned(&["--delete", "--confirm-quiescent"]));
    assert!(f.root.join(&f.old).join("xshelld").exists());
}

#[test]
fn maintenance_does_not_inherit_home_or_prepare_server_state() {
    let temp = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_xshelld"))
        .args(["maintenance", "cleanup-installs"])
        .env("HOME", temp.path())
        .env("XSHELLD_HOME", temp.path())
        .env_remove("XSHELL_EVENT_SOCKET")
        .env_remove("XSHELL_TERMINAL_ID")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0);
    let out = Command::new(env!("CARGO_BIN_EXE_xshelld"))
        .args(["maintenance", "cleanup-installs", "--home"])
        .arg(temp.path())
        .env_remove("XSHELL_EVENT_SOCKET")
        .env_remove("XSHELL_TERMINAL_ID")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0);
}
