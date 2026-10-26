//! Windows build selection uses the same directory-only record as Unix, but invalid
//! records are errors: silently ignoring one could restart an unexpected GUI-bound build.
use super::{valid_dir_name, SELECTED};
use crate::private_fs;
use std::io::{self, Read, Write};
use std::os::windows::fs::MetadataExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use xshell_protocol::build::{install_dir_name, scan_stamp, BuildId};

pub const MAX_BINARY: u64 = 64 * 1024 * 1024;

#[derive(Debug)]
pub struct NativeIdentity {
    pub version: String,
    pub build: BuildId,
}

/// Reject every traversed reparse point, not just the final private object.
pub fn no_reparse(path: &Path) -> io::Result<()> {
    for ancestor in path.ancestors() {
        let m = std::fs::symlink_metadata(ancestor)?;
        if m.file_attributes() & 0x400 != 0 {
            return Err(io::Error::other(format!(
                "{} is a reparse point",
                ancestor.display()
            )));
        }
    }
    Ok(())
}

pub fn windows_server_root(home: &Path) -> io::Result<PathBuf> {
    no_reparse(home)?;
    private_fs::check_owned_dir(home)?;
    let mut dir = home.to_path_buf();
    for part in [".xshell", "server"] {
        dir.push(part);
        private_fs::ensure_dir(&dir)?;
    }
    Ok(dir)
}

/// Native execution is deadline- and output-bounded; a nonzero exit never authenticates a
/// target, even if it printed plausible JSON first. The byte stamp must agree too.
pub fn native_identity(file: &Path) -> io::Result<NativeIdentity> {
    no_reparse(file)?;
    // Installed bundles can be administrator-owned. Staged/selected callers enforce
    // current-user private paths separately; do not rewrite the bundle's DACL here.
    let f = std::fs::File::open(file)?;
    if !f.metadata()?.is_file() {
        return Err(io::Error::other("Daemon image is not a regular file"));
    }
    if f.metadata()?.len() > MAX_BINARY {
        return Err(io::Error::other("Daemon image exceeds 64 MiB"));
    }
    let mut bytes = Vec::new();
    f.take(MAX_BINARY + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_BINARY {
        return Err(io::Error::other("Daemon image exceeds 64 MiB"));
    }
    let stamp = scan_stamp(&bytes)
        .map_err(io::Error::other)?
        .ok_or_else(|| io::Error::other("Daemon build stamp is unknown"))?;
    let mut child = Command::new(file)
        .arg("--version")
        .creation_flags(0x0800_0000)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let stdout = child.stdout.take().expect("piped");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout.take(8193).read_to_end(&mut bytes).map(|_| bytes);
        let _ = tx.send(result);
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Daemon --version timed out",
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let bytes = rx
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|_| io::Error::other("Daemon version output timed out"))??;
    if !status.success() || bytes.len() > 8192 {
        return Err(io::Error::other("Daemon version probe failed"));
    }
    let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    let build: BuildId =
        serde_json::from_value(value["build"].clone()).map_err(io::Error::other)?;
    let range: xshell_protocol::msg::ProtocolRange =
        serde_json::from_value(value["protocol"].clone()).map_err(io::Error::other)?;
    xshell_protocol::negotiate::negotiate(xshell_protocol::PROTOCOL, range)
        .map_err(|e| io::Error::other(format!("{e:?}")))?;
    if value["name"] != "xshelld"
        || value["os"] != "windows"
        || value["arch"] != "x86_64"
        || !stamp.same(&build)
    {
        return Err(io::Error::other("invalid native Daemon identity"));
    }
    let version = value["version"]
        .as_str()
        .ok_or_else(|| io::Error::other("missing version"))?
        .to_string();
    semver::Version::parse(&version).map_err(io::Error::other)?;
    Ok(NativeIdentity { version, build })
}

fn private_install_path(home: &Path, name: &str) -> io::Result<PathBuf> {
    if !valid_dir_name(name) {
        return Err(io::Error::other("invalid install directory"));
    }
    let root = windows_server_root(home)?;
    let dir = root.join(name);
    no_reparse(&dir)?;
    private_fs::ensure_dir(&dir)?;
    let file = dir.join("xshelld.exe");
    private_fs::open_read(&file)?.ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
    Ok(file)
}

pub fn windows_install(home: &Path, name: &str) -> io::Result<PathBuf> {
    let file = private_install_path(home, name)?;
    let id = native_identity(&file)?;
    if install_dir_name(&id.version, Some(&id.build)) != name {
        return Err(io::Error::other(
            "install directory does not match Daemon identity",
        ));
    }
    Ok(file)
}

/// Commit a previously identity-validated install (the selected-update transaction checks
/// it before taking its registry guard). Only bounded private-file operations run here;
/// native execution must never happen while the registry is locked.
pub fn write_windows_selected(home: &Path, name: &str) -> io::Result<()> {
    private_install_path(home, name)?;
    let root = windows_server_root(home)?;
    // Existing records too must be owned by this user and not reparse points.
    let _ = private_fs::open_read(&root.join(SELECTED))?;
    let tmp = root.join(format!(".selected.{}.tmp", uuid::Uuid::new_v4().simple()));
    let result = (|| {
        let mut f = private_fs::create_new(&tmp)?;
        writeln!(f, "{name}")?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, root.join(SELECTED))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(tmp);
    }
    result
}

pub fn windows_spawn_target(home: &Path, bundled: &Path) -> io::Result<PathBuf> {
    let own = native_identity(bundled)?;
    // Selection is opt-in state. Without a record, use the validated bundle without
    // creating update directories or imposing their current-user ownership policy on
    // an otherwise unrelated HOME (including elevated Windows profiles).
    match std::fs::symlink_metadata(super::server_root(home).join(SELECTED)) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(bundled.into()),
        Err(error) => return Err(error),
        Ok(_) => {}
    }
    let root = windows_server_root(home)?;
    let Some(record) = private_fs::open_read(&root.join(SELECTED))? else {
        return Ok(bundled.into());
    };
    let mut text = String::new();
    record.take(257).read_to_string(&mut text)?;
    let name = text.strip_suffix('\n').unwrap_or(&text);
    if text.len() > 256 || !valid_dir_name(name) {
        return Err(io::Error::other("invalid selected Daemon record"));
    }
    let selected = windows_install(home, name)?;
    let identity = native_identity(&selected)?;
    let selected_version = semver::Version::parse(&identity.version).map_err(io::Error::other)?;
    let own_version = semver::Version::parse(&own.version).map_err(io::Error::other)?;
    Ok(if selected_version.cmp_precedence(&own_version).is_lt() {
        bundled.into()
    } else {
        selected
    })
}
