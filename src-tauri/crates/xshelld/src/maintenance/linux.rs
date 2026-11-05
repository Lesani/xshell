use super::Cleanup;
use std::collections::BTreeMap;
use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::{self, File, Metadata};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

// Refuse partial inventories. Includes binaries; no candidate is ever executed.
const MAX_CHILDREN: usize = 256;
const MAX_PROCESSES: usize = 4096;
const MAX_BINARY: usize = 128 * 1024 * 1024;
const MAX_SMALL: usize = 1024 * 1024;
const MAX_TOTAL: usize = 512 * 1024 * 1024;
const DEADLINE: Duration = Duration::from_secs(30);
type Result<T> = std::result::Result<T, String>;
fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}
fn fail<T>(message: &str) -> Result<T> {
    Err(message.into())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Identity {
    dev: u64,
    ino: u64,
    uid: u32,
    mode: u32,
    links: u64,
    len: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}
impl From<Metadata> for Identity {
    fn from(m: Metadata) -> Self {
        Self {
            dev: m.dev(),
            ino: m.ino(),
            uid: m.uid(),
            mode: m.mode(),
            links: m.nlink(),
            len: m.len(),
            mtime: (m.mtime(), m.mtime_nsec()),
            ctime: (m.ctime(), m.ctime_nsec()),
        }
    }
}
fn same_inode(a: &Identity, b: &Identity) -> bool {
    a.dev == b.dev && a.ino == b.ino
}
fn private(m: &Metadata) -> bool {
    m.uid() == unsafe { libc::getuid() } && m.mode() & 0o022 == 0
}
fn cstr(s: &OsStr) -> Result<CString> {
    CString::new(s.as_bytes()).map_err(err)
}
fn file_from_fd(fd: libc::c_long) -> io::Result<File> {
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd as RawFd) })
    }
}
fn open_at(dir: RawFd, name: &OsStr, directory: bool) -> Result<File> {
    let name = cstr(name)?;
    let flags = libc::O_RDONLY
        | libc::O_CLOEXEC
        | libc::O_NOFOLLOW
        | libc::O_NONBLOCK
        | if directory { libc::O_DIRECTORY } else { 0 };
    file_from_fd(unsafe { libc::openat(dir, name.as_ptr(), flags) } as _).map_err(err)
}

#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}
const RESOLVE_NO_XDEV: u64 = 0x01;
const RESOLVE_NO_SYMLINKS: u64 = 0x04;
const RESOLVE_BENEATH: u64 = 0x08;

// No fallback: st_dev alone cannot distinguish same-filesystem bind mounts.
fn mount_open(dir: RawFd, path: &OsStr, directory: bool) -> io::Result<File> {
    let name = CString::new(path.as_bytes())?;
    let how = OpenHow {
        flags: (libc::O_RDONLY
            | libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | if directory { libc::O_DIRECTORY } else { 0 }) as u64,
        mode: 0,
        resolve: RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_XDEV,
    };
    file_from_fd(unsafe {
        libc::syscall(
            libc::SYS_openat2,
            dir,
            name.as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        )
    })
}

/// Narrow syscall/read seams for refusal and partial-removal tests; never environment toggles.
trait Io {
    fn pid1_namespace(&self) -> io::Result<PathBuf> {
        fs::read_link("/proc/1/ns/pid")
    }
    fn process_entries(&self) -> Result<Vec<OsString>> {
        names(&File::open("/proc").map_err(err)?, MAX_PROCESSES + 256)
    }
    fn after_inventory(&self) -> Result<()> {
        Ok(())
    }
    fn beneath(&self, dir: RawFd, path: &OsStr, directory: bool) -> io::Result<File> {
        mount_open(dir, path, directory)
    }
    fn proc_read(&self, path: &Path, limit: usize) -> Result<Vec<u8>> {
        bounded(File::open(path).map_err(err)?, limit)
    }
    fn unlink(&self, dir: RawFd, name: &CStr, flags: i32) -> io::Result<()> {
        if unsafe { libc::unlinkat(dir, name.as_ptr(), flags) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}
struct Native;
impl Io for Native {}

struct Budget {
    start: Instant,
    bytes: usize,
}
impl Budget {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            bytes: 0,
        }
    }
    fn check(&self) -> Result<()> {
        if self.start.elapsed() > DEADLINE {
            fail("inventory deadline exceeded")
        } else {
            Ok(())
        }
    }
    fn charge(&mut self, n: usize) -> Result<()> {
        self.bytes = self
            .bytes
            .checked_add(n)
            .ok_or("inventory byte bound exceeded")?;
        if self.bytes > MAX_TOTAL {
            return fail("inventory byte bound exceeded");
        }
        self.check()
    }
    fn read(&mut self, file: File, cap: usize) -> Result<Vec<u8>> {
        self.check()?;
        let bytes = bounded(file, cap)?;
        self.charge(bytes.len())?;
        Ok(bytes)
    }
    fn proc(&mut self, io: &impl Io, path: &Path, cap: usize) -> Result<Vec<u8>> {
        self.check()?;
        let bytes = io.proc_read(path, cap)?;
        if bytes.len() > cap {
            return fail("process read bound exceeded");
        }
        self.charge(bytes.len())?;
        Ok(bytes)
    }
}
fn bounded(file: File, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(err)?;
    if bytes.len() > limit {
        fail("read bound exceeded")
    } else {
        Ok(bytes)
    }
}

// Every component is opened without following links; only managed ancestors need be private.
fn unaliased_dir(path: &Path) -> Result<File> {
    let mut dir = File::open("/").map_err(err)?;
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => dir = open_at(dir.as_raw_fd(), name, true)?,
            _ => return fail("non-absolute or ambiguous path"),
        }
    }
    Ok(dir)
}
fn absolute_dir(path: &Path) -> Result<File> {
    let dir = unaliased_dir(path)?;
    let m = dir.metadata().map_err(err)?;
    if !m.is_dir() || !private(&m) {
        return fail("home must be a private same-UID directory");
    }
    Ok(dir)
}
fn private_dir(dir: &File, name: &str) -> Result<File> {
    let file = open_at(dir.as_raw_fd(), OsStr::new(name), true)?;
    if !private(&file.metadata().map_err(err)?) {
        return fail("managed directory is not private and same-UID");
    }
    Ok(file)
}
fn local(dir: &File) -> Result<()> {
    let mut st = std::mem::MaybeUninit::<libc::statfs>::uninit();
    if unsafe { libc::fstatfs(dir.as_raw_fd(), st.as_mut_ptr()) } != 0 {
        return Err(err(io::Error::last_os_error()));
    }
    let magic = unsafe { st.assume_init() }.f_type as u64;
    // Explicit local filesystem allowlist: ext*, XFS, Btrfs, tmpfs, ZFS, overlayfs.
    if !matches!(
        magic,
        0xef53 | 0x58465342 | 0x9123683e | 0x01021994 | 0x2fc12fc1 | 0x794c7630
    ) {
        return fail("unsupported or potentially shared filesystem");
    }
    Ok(())
}
fn names(dir: &File, cap: usize) -> Result<Vec<OsString>> {
    // Open a fresh directory description, not dup's shared readdir offset.
    let fd = open_at(dir.as_raw_fd(), OsStr::new("."), true)?;
    let raw = unsafe { libc::fdopendir(libc::dup(fd.as_raw_fd())) };
    if raw.is_null() {
        return Err(err(io::Error::last_os_error()));
    }
    let result = (|| {
        let mut out = Vec::new();
        loop {
            unsafe {
                *libc::__errno_location() = 0;
            }
            let entry = unsafe { libc::readdir(raw) };
            if entry.is_null() {
                if io::Error::last_os_error().raw_os_error() != Some(0) {
                    return Err(err(io::Error::last_os_error()));
                }
                break;
            }
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
            if name == b"." || name == b".." {
                continue;
            }
            if out.len() == cap {
                return fail("directory child bound exceeded");
            }
            out.push(OsString::from_vec(name.to_vec()));
        }
        out.sort();
        Ok(out)
    })();
    unsafe {
        libc::closedir(raw);
    }
    result
}
fn at_identity(dir: &File, name: &OsStr) -> Result<Identity> {
    let name = cstr(name)?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(err(io::Error::last_os_error()));
    }
    let s = unsafe { stat.assume_init() };
    Ok(Identity {
        dev: s.st_dev,
        ino: s.st_ino,
        uid: s.st_uid,
        mode: s.st_mode,
        links: s.st_nlink,
        len: s.st_size as u64,
        mtime: (s.st_mtime, s.st_mtime_nsec),
        ctime: (s.st_ctime, s.st_ctime_nsec),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Record {
    parents: Vec<Identity>,
    file: Option<Identity>,
    bytes: Option<Vec<u8>>,
}
fn optional_record(
    dir: &File,
    name: &str,
    budget: &mut Budget,
    parents: Vec<Identity>,
) -> Result<Record> {
    let identity = match at_identity(dir, OsStr::new(name)) {
        Ok(id) => id,
        Err(_) => {
            // Only ENOENT is optional, never unreadable, symlinked or malformed records.
            let name = cstr(OsStr::new(name))?;
            let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
            if unsafe {
                libc::fstatat(
                    dir.as_raw_fd(),
                    name.as_ptr(),
                    st.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } == -1
                && io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT)
            {
                return Ok(Record {
                    parents,
                    file: None,
                    bytes: None,
                });
            }
            return fail("cannot inspect reference record");
        }
    };
    let file = open_at(dir.as_raw_fd(), OsStr::new(name), false)?;
    let m = file.metadata().map_err(err)?;
    if !m.is_file() || !private(&m) || m.nlink() != 1 || Identity::from(m) != identity {
        return fail("unsafe reference record");
    }
    let bytes = budget.read(file, MAX_SMALL)?;
    Ok(Record {
        parents,
        file: Some(identity),
        bytes: Some(bytes),
    })
}
fn selected(root: &File, budget: &mut Budget) -> Result<Record> {
    let record = optional_record(root, "selected", budget, vec![])?;
    if let Some(bytes) = &record.bytes {
        let text = std::str::from_utf8(bytes).map_err(err)?;
        let name = text.strip_suffix('\n').unwrap_or(text);
        if !xshell_core::server::valid_dir_name(name) {
            return fail("malformed raw selected record");
        }
    }
    Ok(record)
}
fn hook_record(home: &Path, budget: &mut Budget) -> Result<Record> {
    let mut dir = absolute_dir(home)?;
    local(&dir)?;
    let mut parents = vec![dir.metadata().map_err(err)?.into()];
    for part in [".xshell", "daemon"] {
        match at_identity(&dir, OsStr::new(part)) {
            Ok(_) => {
                dir = private_dir(&dir, part)?;
                local(&dir)?;
                parents.push(dir.metadata().map_err(err)?.into());
            }
            Err(_) => {
                // Reuse the strict optional read's missing/denied distinction on the parent.
                let record = optional_record(&dir, part, budget, parents)?;
                if record.file.is_none() {
                    return Ok(record);
                }
                return fail("unsafe hook ancestor");
            }
        }
    }
    optional_record(&dir, "claude-hooks.json", budget, parents)
}

// Recognize exactly the generated format, including POSIX quoting, without shell evaluation.
fn hook_executable(bytes: &[u8]) -> Result<PathBuf> {
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(err)?;
    let command = value
        .pointer("/hooks/Stop/0/hooks/0/command")
        .and_then(|v| v.as_str())
        .ok_or("unrecognized hook settings")?;
    let word = command
        .strip_suffix(" event - finished")
        .ok_or("unrecognized hook command")?;
    let quoted = word
        .strip_prefix('\'')
        .and_then(|w| w.strip_suffix('\''))
        .ok_or("unrecognized hook quoting")?;
    let decoded = quoted.replace("'\\''", "'");
    if xshell_core::agent_status::posix_quote(&decoded) != word {
        return fail("unrecognized hook quoting");
    }
    let path = PathBuf::from(decoded);
    if !super::absolute(&path) {
        return fail("unresolved hook path alias");
    }
    let expected: serde_json::Value =
        serde_json::from_str(&xshell_core::agent_status::claude_settings_json(&path))
            .map_err(err)?;
    if value != expected {
        return fail("custom or unrecognized hook settings");
    }
    Ok(path)
}
fn hook_target(path: &Path, target: &Path, io: &impl Io) -> Result<(Option<String>, Identity)> {
    // Validate even external targets; aliases/custom wrappers are not guessed.
    let parent = path.parent().ok_or("hook has no parent")?;
    let dir = absolute_dir(parent)?;
    let file = open_at(
        dir.as_raw_fd(),
        path.file_name().ok_or("hook has no filename")?,
        false,
    )?;
    let m = file.metadata().map_err(err)?;
    if !m.is_file() || !private(&m) || m.nlink() != 1 || m.mode() & 0o100 == 0 {
        return fail("unresolved or unsafe hook executable");
    }
    let identity: Identity = m.into();
    if path.starts_with(target) {
        let relative = path.strip_prefix(target).map_err(err)?;
        let parts: Vec<_> = relative.components().collect();
        if parts.len() != 2 || parts[1].as_os_str() != "xshelld" {
            return fail("unrecognized target-root hook path");
        }
        let root = absolute_dir(target)?;
        let opened = io
            .beneath(root.as_raw_fd(), relative.as_os_str(), false)
            .map_err(|e| format!("hook mount boundary: {e}"))?;
        if Identity::from(opened.metadata().map_err(err)?) != identity {
            return fail("hook identity changed");
        }
        let name = parts[0]
            .as_os_str()
            .to_str()
            .ok_or("non-UTF8 hook install")?;
        if !xshell_core::server::valid_dir_name(name) {
            return fail("ambiguous hook install name");
        }
        return Ok((Some(name.to_string()), identity));
    }
    Ok((None, identity))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Candidate {
    name: String,
    directory: Identity,
    executable: Identity,
}
#[derive(Debug, Clone, PartialEq, Eq)]
struct Process {
    start: u64,
    parent: u32,
    exe: Identity,
    argv: Vec<u8>,
    env: Vec<u8>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
struct Inventory {
    ancestors: Vec<Identity>,
    children: Vec<(OsString, Identity)>,
    selected: Record,
    hooks: Vec<(Record, Option<Identity>)>,
    candidates: Vec<Candidate>,
    processes: BTreeMap<u32, Process>,
    keeps: BTreeMap<OsString, &'static str>,
}
fn keyed(name: &str, bytes: &[u8]) -> bool {
    let Some((version, _)) = name.split_once('+') else {
        return false;
    };
    let Ok(version_parsed) = semver::Version::parse(version) else {
        return false;
    };
    if version_parsed.to_string() != version {
        return false;
    }
    let Ok(Some(build)) = xshell_protocol::build::scan_stamp(bytes) else {
        return false;
    };
    xshell_protocol::build::install_dir_name(version, Some(&build)) == name
}
fn candidate(
    root: &File,
    name: &str,
    budget: &mut Budget,
    io: &impl Io,
) -> Result<Option<Candidate>> {
    if !xshell_core::server::valid_dir_name(name) || !name.contains('+') {
        return Ok(None);
    }
    let dir = io
        .beneath(root.as_raw_fd(), OsStr::new(name), true)
        .map_err(|e| format!("candidate mount boundary: {e}"))?;
    let m = dir.metadata().map_err(err)?;
    if !private(&m) || names(&dir, MAX_CHILDREN)? != [OsString::from("xshelld")] {
        return Ok(None);
    }
    let identity = at_identity(&dir, OsStr::new("xshelld"))?;
    if identity.mode & libc::S_IFMT != libc::S_IFREG
        || identity.uid != unsafe { libc::getuid() }
        || identity.mode & 0o022 != 0
        || identity.mode & 0o100 == 0
        || identity.links != 1
    {
        return Ok(None);
    }
    let file = io
        .beneath(
            root.as_raw_fd(),
            OsStr::new(&format!("{name}/xshelld")),
            false,
        )
        .map_err(|e| format!("executable mount boundary: {e}"))?;
    if Identity::from(file.metadata().map_err(err)?) != identity {
        return fail("candidate executable changed");
    }
    if !keyed(name, &budget.read(file, MAX_BINARY)?) {
        return Ok(None);
    }
    Ok(Some(Candidate {
        name: name.into(),
        directory: m.into(),
        executable: identity,
    }))
}

fn stat_identity(bytes: &[u8]) -> Result<(u32, u64)> {
    let text = std::str::from_utf8(bytes).map_err(err)?;
    let (_, tail) = text.rsplit_once(") ").ok_or("malformed process stat")?;
    let fields: Vec<_> = tail.split_whitespace().collect();
    if fields.len() < 20 {
        return fail("truncated process stat");
    }
    Ok((
        fields[1].parse().map_err(err)?,
        fields[19].parse().map_err(err)?,
    ))
}
fn nul_vector(bytes: &[u8]) -> Result<()> {
    if bytes.last() != Some(&0) {
        return fail("empty or truncated process vector");
    }
    Ok(())
}
fn namespace_visibility(budget: &mut Budget, io: &impl Io) -> Result<()> {
    if unsafe { libc::getuid() != libc::geteuid() } {
        return fail("setuid maintenance is unsupported");
    }
    let own_namespace = fs::read_link("/proc/self/ns/pid").map_err(err)?;
    match io.pid1_namespace() {
        Ok(namespace) if namespace != own_namespace => {
            return fail("inconsistent PID namespace visibility")
        }
        Ok(_) => {}
        // Linux ptrace permissions normally hide root PID1's namespace from an unprivileged
        // UID. This is not a denied same-UID process read, nor proof of complete visibility.
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {}
        Err(e) => return Err(err(e)),
    }
    let init = budget.proc(io, Path::new("/proc/1/status"), MAX_SMALL)?;
    let init = std::str::from_utf8(&init).map_err(err)?;
    if init
        .lines()
        .find(|s| s.starts_with("NSpid:"))
        .is_none_or(|s| s.split_whitespace().collect::<Vec<_>>() != ["NSpid:", "1"])
    {
        return fail("inconsistent PID1 namespace status");
    }
    let status = budget.proc(io, Path::new("/proc/self/status"), MAX_SMALL)?;
    let text = std::str::from_utf8(&status).map_err(err)?;
    if text
        .lines()
        .find(|s| s.starts_with("NSpid:"))
        .is_none_or(|s| s.split_whitespace().count() != 2)
    {
        return fail("nested or unknown PID namespace visibility");
    }
    let uid_map = budget.proc(io, Path::new("/proc/self/uid_map"), 1024)?;
    if std::str::from_utf8(&uid_map)
        .map_err(err)?
        .split_whitespace()
        .collect::<Vec<_>>()
        != ["0", "0", "4294967295"]
    {
        return fail("user namespace visibility is incomplete");
    }
    let mountinfo = budget.proc(io, Path::new("/proc/self/mountinfo"), MAX_SMALL)?;
    let text = std::str::from_utf8(&mountinfo).map_err(err)?;
    let mut proc_mounts = 0;
    for line in text.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.get(4) == Some(&"/proc") {
            let (_, tail) = line.split_once(" - ").ok_or("malformed proc mount")?;
            let fields: Vec<_> = tail.split_whitespace().collect();
            if fields.first() != Some(&"proc")
                || fields.get(2).is_none_or(|s| {
                    s.split(',').any(|s| {
                        s.starts_with("hidepid=") && s != "hidepid=0" || s.starts_with("subset=")
                    })
                })
            {
                return fail("restricted process mount");
            }
            proc_mounts += 1;
        }
    }
    if proc_mounts != 1 {
        return fail("unknown process mount visibility");
    }
    Ok(())
}
fn product_reference(exe: &Path, argv: &[u8], env: &[u8], target: &Path) -> bool {
    exe.file_name()
        .is_some_and(|n| n == "xshelld" || n == "xshell")
        || exe.starts_with(target)
        || argv
            .windows(target.as_os_str().len())
            .any(|w| w == target.as_os_str().as_bytes())
        || env.split(|b| *b == 0).any(|v| {
            v.starts_with(b"XSHELL_EVENT_EXE=")
                || v.starts_with(b"XSHELL_TERMINAL_ID=")
                || v.starts_with(b"XSHELL_EVENT_SOCKET=")
        })
}
fn processes(
    budget: &mut Budget,
    io: &impl Io,
    target: &Path,
    current: &Identity,
) -> Result<BTreeMap<u32, Process>> {
    namespace_visibility(budget, io)?;
    let mut ancestry = BTreeMap::new();
    let mut pid = std::process::id();
    while pid != 0 {
        if ancestry.len() >= 128 || ancestry.contains_key(&pid) {
            return fail("invalid or excessive invocation ancestry");
        }
        let bytes = budget.proc(io, &PathBuf::from(format!("/proc/{pid}/stat")), 16384)?;
        let identity = stat_identity(&bytes)?;
        ancestry.insert(pid, identity);
        pid = identity.0;
    }
    let entries = io.process_entries()?;
    if entries.len() > MAX_PROCESSES + 256 {
        return fail("process entry bound exceeded");
    }
    let mut count = 0;
    let mut out = BTreeMap::new();
    for name in entries {
        let Some(pid) = name.to_str().and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        count += 1;
        if count > MAX_PROCESSES {
            return fail("process bound exceeded");
        }
        budget.check()?;
        let path = PathBuf::from("/proc").join(name);
        // /proc/PID ownership is NOT a UID filter: a nondumpable same-UID process's
        // directory can be owned by root. Read the actual credentials for every PID.
        let status = match budget.proc(io, &path.join("status"), MAX_SMALL) {
            Ok(status) => status,
            Err(error) => match fs::symlink_metadata(&path) {
                // readdir can race an already-completed exit. Resolve it explicitly;
                // never ignore a failed read of a still-present or relevant process.
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                _ => return Err(error),
            },
        };
        let status = std::str::from_utf8(&status).map_err(err)?;
        let ids = status
            .lines()
            .find_map(|s| s.strip_prefix("Uid:"))
            .ok_or("unknown process UID")?;
        let ids = ids
            .split_whitespace()
            .map(|id| id.parse::<u32>().map_err(err))
            .collect::<Result<Vec<_>>>()?;
        if ids.len() != 4 {
            return fail("truncated process credentials");
        }
        let uid = unsafe { libc::getuid() };
        if !ids.contains(&uid) {
            continue;
        }
        if ids.iter().any(|id| *id != uid) {
            return fail("ambiguous process credentials");
        }
        let stat = budget.proc(io, &path.join("stat"), 16384)?;
        let (parent, start) = stat_identity(&stat)?;
        if ancestry.get(&pid).is_some_and(|id| *id != (parent, start)) {
            return fail("invocation ancestry changed");
        }
        for ns in ["pid", "mnt"] {
            if fs::read_link(path.join(format!("ns/{ns}"))).map_err(err)?
                != fs::read_link(format!("/proc/self/ns/{ns}")).map_err(err)?
            {
                return fail("same-UID process in another namespace");
            }
        }
        let exe_path = fs::read_link(path.join("exe"))
            .map_err(|_| "unresolved or exited process executable")?;
        if exe_path.as_os_str().as_bytes().ends_with(b" (deleted)") {
            return fail("deleted process executable");
        }
        let exe: Identity = fs::metadata(path.join("exe")).map_err(err)?.into();
        let argv = budget.proc(io, &path.join("cmdline"), MAX_SMALL)?;
        let env = budget.proc(io, &path.join("environ"), MAX_SMALL)?;
        nul_vector(&argv)?;
        // Empty environments are valid, unlike empty cmdlines (zombies/kernel tasks).
        if !env.is_empty() {
            nul_vector(&env)?;
        }
        let after = budget.proc(io, &path.join("stat"), 16384)?;
        if stat_identity(&after)? != (parent, start)
            || Identity::from(fs::metadata(path.join("exe")).map_err(err)?) != exe
        {
            return fail("process identity changed");
        }
        if pid == std::process::id() {
            if !same_inode(&exe, current) {
                return fail("current executable identity changed");
            }
        } else {
            if product_reference(&exe_path, &argv, &env, target) {
                return fail("observed Daemon, connector, installer or hook consumer; no deletion");
            }
            if !ancestry.contains_key(&pid) {
                return fail("unknown same-UID process outside invocation ancestry; no deletion");
            }
        }
        out.insert(
            pid,
            Process {
                start,
                parent,
                exe,
                argv,
                env,
            },
        );
    }
    if !out.contains_key(&std::process::id()) {
        return fail("current process missing from inventory");
    }
    for (pid, identity) in ancestry {
        let bytes = budget.proc(io, &PathBuf::from(format!("/proc/{pid}/stat")), 16384)?;
        if stat_identity(&bytes)? != identity {
            return fail("invocation ancestry changed during inventory");
        }
    }
    Ok(out)
}

fn inventory(opts: &Cleanup, budget: &mut Budget, io: &impl Io) -> Result<(Inventory, File)> {
    let home = absolute_dir(&opts.home)?;
    let managed = private_dir(&home, ".xshell")?;
    let root = private_dir(&managed, "server")?;
    for dir in [&home, &managed, &root] {
        local(dir)?;
    }
    io.beneath(root.as_raw_fd(), OsStr::new("."), true)
        .map_err(|e| format!("mount boundary unavailable; no fallback: {e}"))?;
    let ancestors = [&home, &managed, &root]
        .iter()
        .map(|f| f.metadata().map(Identity::from).map_err(err))
        .collect::<Result<Vec<_>>>()?;
    let target = opts.home.join(".xshell/server");
    let current_path = fs::read_link("/proc/self/exe").map_err(err)?;
    if current_path.as_os_str().as_bytes().ends_with(b" (deleted)") {
        return fail("current executable was deleted");
    }
    let current: Identity = fs::metadata("/proc/self/exe").map_err(err)?.into();
    let current_parent = unaliased_dir(
        current_path
            .parent()
            .ok_or("unresolved current executable")?,
    )?;
    let current_file = open_at(
        current_parent.as_raw_fd(),
        current_path
            .file_name()
            .ok_or("unresolved current executable")?,
        false,
    )?;
    if !same_inode(
        &current,
        &Identity::from(current_file.metadata().map_err(err)?),
    ) {
        return fail("unresolved current executable identity");
    }
    let selected = selected(&root, budget)?;
    let mut protected = opts.keep.clone();
    if let Some(bytes) = &selected.bytes {
        let text = std::str::from_utf8(bytes).map_err(err)?;
        protected.insert(text.trim_end_matches('\n').to_owned());
    }
    let mut hooks = Vec::new();
    for reference in std::iter::once(&opts.home).chain(opts.reference_homes.iter()) {
        let record = hook_record(reference, budget)?;
        let executable = if let Some(bytes) = &record.bytes {
            let (name, executable) = hook_target(&hook_executable(bytes)?, &target, io)?;
            if let Some(name) = name {
                protected.insert(name);
            }
            Some(executable)
        } else {
            None
        };
        hooks.push((record, executable));
    }
    let mut children = Vec::new();
    let mut candidates = Vec::new();
    let mut keeps = BTreeMap::new();
    for name in names(&root, MAX_CHILDREN)? {
        budget.check()?;
        let identity = at_identity(&root, &name)?;
        children.push((name.clone(), identity.clone()));
        if name == "selected" {
            continue;
        }
        if protected.contains(name.to_str().unwrap_or("")) {
            keeps.insert(name, "selected, hook-referenced or explicitly kept");
        } else if current_path.parent() == Some(target.join(&name).as_path()) {
            keeps.insert(name, "current executable install");
        } else if identity.mode & libc::S_IFMT != libc::S_IFDIR
            || identity.uid != unsafe { libc::getuid() }
            || identity.mode & 0o022 != 0
        {
            keeps.insert(name, "ambiguous or unsafe layout");
        } else if let Some(c) = match name.to_str() {
            Some(n) => candidate(&root, n, budget, io)?,
            None => None,
        } {
            if same_inode(&c.executable, &current) {
                keeps.insert(name, "current executable inode");
            } else if hooks.iter().any(|(_, exe)| {
                exe.as_ref()
                    .is_some_and(|exe| same_inode(exe, &c.executable))
            }) {
                return fail(
                    "hook executable aliases a target install through an unrecognized path",
                );
            } else {
                candidates.push(c);
            }
        } else {
            keeps.insert(name, "legacy, unknown build or ambiguous layout");
        }
    }
    let processes = processes(budget, io, &target, &current)?;
    Ok((
        Inventory {
            ancestors,
            children,
            selected,
            hooks,
            candidates,
            processes,
            keeps,
        },
        root,
    ))
}

fn remove(
    root: &File,
    candidates: &[Candidate],
    io: &impl Io,
    report: &mut Vec<String>,
) -> Result<()> {
    for c in candidates {
        let dir = io
            .beneath(root.as_raw_fd(), OsStr::new(&c.name), true)
            .map_err(|e| format!("mount boundary before unlink: {e}"))?;
        let file = io
            .beneath(
                root.as_raw_fd(),
                OsStr::new(&format!("{}/xshelld", c.name)),
                false,
            )
            .map_err(|e| format!("mount boundary before unlink: {e}"))?;
        if Identity::from(dir.metadata().map_err(err)?) != c.directory
            || Identity::from(file.metadata().map_err(err)?) != c.executable
            || at_identity(root, OsStr::new(&c.name))? != c.directory
            || names(&dir, MAX_CHILDREN)? != [OsString::from("xshelld")]
        {
            return fail("candidate changed before unlink; stopped");
        }
        io.unlink(dir.as_raw_fd(), c"xshelld", 0)
            .map_err(|e| format!("unlink failed for {}: {e}; stopped", c.name))?;
        // A directory failure is partial deletion, never a rollback or a removed install.
        report.push(format!(
            "removed-executable {} (directory removal pending)",
            c.name
        ));
        io.unlink(
            root.as_raw_fd(),
            &cstr(OsStr::new(&c.name))?,
            libc::AT_REMOVEDIR,
        )
        .map_err(|e| {
            format!(
                "partial removal {}: executable removed, directory remains: {e}; stopped",
                c.name
            )
        })?;
        report.pop();
        report.push(format!("removed {}", c.name));
    }
    Ok(())
}

fn execute(opts: &Cleanup, io: &impl Io, report: &mut Vec<String>) -> Result<()> {
    let mut budget = Budget::new();
    let (first, _) = inventory(opts, &mut budget, io)?;
    for (name, reason) in &first.keeps {
        report.push(format!("keep {name:?}: {reason}"));
    }
    for c in &first.candidates {
        report.push(format!("would-remove {}", c.name));
    }
    if !opts.delete {
        return Ok(());
    }
    io.after_inventory()?;
    let (second, root) = inventory(opts, &mut budget, io)?;
    if first != second {
        return fail("inventory changed during recheck; no deletion");
    }
    budget.check()?;
    remove(&root, &second.candidates, io, report)
}
pub(super) fn run(opts: Cleanup) -> i32 {
    let mut report = Vec::new();
    let result = execute(&opts, &Native, &mut report);
    for line in report {
        println!("{line}");
    }
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("blocked: {e}; assessment incomplete (would-remove entries are tentative)");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::collections::BTreeSet;
    use std::os::unix::fs::PermissionsExt;

    struct OneProcess;
    impl Io for OneProcess {
        fn process_entries(&self) -> Result<Vec<OsString>> {
            Ok(vec![std::process::id().to_string().into()])
        }
    }
    fn fixture() -> (tempfile::TempDir, Cleanup) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join(".xshell/server");
        fs::create_dir_all(&root).unwrap();
        for path in [temp.path(), &temp.path().join(".xshell"), &root] {
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        for (name, commit) in [
            ("1.0.0+111111111111", "111111111111"),
            ("1.0.0+222222222222", "222222222222"),
        ] {
            fs::create_dir(root.join(name)).unwrap();
            fs::set_permissions(root.join(name), fs::Permissions::from_mode(0o700)).unwrap();
            let exe = root.join(name).join("xshelld");
            fs::write(&exe, format!("@@XSHELL-BUILD-V1:{commit}:1@@")).unwrap();
            fs::set_permissions(exe, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let opts = Cleanup {
            home: temp.path().to_owned(),
            reference_homes: vec![],
            keep: BTreeSet::new(),
            delete: true,
        };
        (temp, opts)
    }
    fn old(opts: &Cleanup) -> PathBuf {
        opts.home.join(".xshell/server/1.0.0+111111111111")
    }

    #[test]
    fn mount_boundary_exdev_and_unsupported_have_no_fallback_or_unlink() {
        struct MountFault {
            code: i32,
            opens: Cell<usize>,
            unlinks: Cell<usize>,
        }
        impl Io for MountFault {
            fn beneath(&self, _: RawFd, _: &OsStr, _: bool) -> io::Result<File> {
                self.opens.set(self.opens.get() + 1);
                Err(io::Error::from_raw_os_error(self.code))
            }
            fn unlink(&self, _: RawFd, _: &CStr, _: i32) -> io::Result<()> {
                self.unlinks.set(self.unlinks.get() + 1);
                panic!("must never unlink")
            }
        }
        for code in [libc::EXDEV, libc::ENOSYS, libc::EINVAL, libc::EPERM] {
            let (_temp, opts) = fixture();
            let outside = opts.home.join("outside-executable");
            fs::write(&outside, b"outside sentinel").unwrap();
            let before = Identity::from(fs::metadata(&outside).unwrap());
            let io = MountFault {
                code,
                opens: Cell::new(0),
                unlinks: Cell::new(0),
            };
            let mut report = vec![];
            assert!(execute(&opts, &io, &mut report)
                .unwrap_err()
                .contains("no fallback"));
            assert_eq!(io.opens.get(), 1);
            assert_eq!(io.unlinks.get(), 0);
            assert!(old(&opts).join("xshelld").exists());
            assert_eq!(Identity::from(fs::metadata(&outside).unwrap()), before);
            assert_eq!(fs::read(outside).unwrap(), b"outside sentinel");
        }
    }

    #[test]
    fn mount_boundary_is_reestablished_before_unlink() {
        struct LateMount;
        impl Io for LateMount {
            fn beneath(&self, _: RawFd, _: &OsStr, _: bool) -> io::Result<File> {
                Err(io::Error::from_raw_os_error(libc::EXDEV))
            }
        }
        let (_temp, opts) = fixture();
        let (inventory, root) = inventory(&opts, &mut Budget::new(), &OneProcess).unwrap();
        assert!(
            remove(&root, &inventory.candidates, &LateMount, &mut vec![])
                .unwrap_err()
                .contains("mount boundary before unlink")
        );
        assert!(old(&opts).join("xshelld").exists());
    }

    #[test]
    fn unlink_and_rmdir_faults_stop_and_report_partial_removal() {
        struct UnlinkFault {
            fail_at: usize,
            calls: Cell<usize>,
        }
        impl Io for UnlinkFault {
            fn unlink(&self, dir: RawFd, name: &CStr, flags: i32) -> io::Result<()> {
                self.calls.set(self.calls.get() + 1);
                if self.calls.get() == self.fail_at {
                    Err(io::Error::from_raw_os_error(libc::EIO))
                } else {
                    Native.unlink(dir, name, flags)
                }
            }
        }
        for fail_at in [1, 2] {
            let (_temp, opts) = fixture();
            let (inventory, root) = inventory(&opts, &mut Budget::new(), &OneProcess).unwrap();
            let io = UnlinkFault {
                fail_at,
                calls: Cell::new(0),
            };
            let mut report = vec![];
            let message = remove(&root, &inventory.candidates, &io, &mut report).unwrap_err();
            assert_eq!(io.calls.get(), fail_at);
            assert!(old(&opts).is_dir());
            assert_eq!(old(&opts).join("xshelld").exists(), fail_at == 1);
            assert!(opts
                .home
                .join(".xshell/server/1.0.0+222222222222/xshelld")
                .exists());
            if fail_at == 1 {
                assert!(report.is_empty());
                assert!(message.contains("unlink failed"));
            } else {
                assert!(message.contains("partial removal"));
                assert!(report[0].starts_with("removed-executable"));
                let (retry, _) = inventory_fn(&opts);
                assert!(retry.keeps.contains_key(OsStr::new("1.0.0+111111111111")));
                assert_eq!(retry.candidates.len(), 1);
            }
        }
    }
    fn inventory_fn(opts: &Cleanup) -> (Inventory, File) {
        inventory(opts, &mut Budget::new(), &OneProcess).unwrap()
    }

    #[test]
    fn inventory_root_selection_and_reference_changes_precede_any_unlink() {
        enum Change {
            Root,
            Selection,
            Reference,
        }
        struct Changed<'a> {
            opts: &'a Cleanup,
            change: Change,
        }
        impl Io for Changed<'_> {
            fn process_entries(&self) -> Result<Vec<OsString>> {
                OneProcess.process_entries()
            }
            fn after_inventory(&self) -> Result<()> {
                match self.change {
                    Change::Root => {
                        fs::write(self.opts.home.join(".xshell/server/new-leaf"), b"new")
                            .map_err(err)?;
                    }
                    Change::Selection => {
                        fs::write(
                            self.opts.home.join(".xshell/server/selected"),
                            b"1.0.0+111111111111\n",
                        )
                        .map_err(err)?;
                    }
                    Change::Reference => {
                        let daemon = self.opts.home.join(".xshell/daemon");
                        fs::create_dir_all(&daemon).map_err(err)?;
                        fs::write(daemon.join("claude-hooks.json"), b"{}").map_err(err)?;
                    }
                }
                Ok(())
            }
        }
        for change in [Change::Root, Change::Selection, Change::Reference] {
            let (_temp, opts) = fixture();
            assert!(execute(
                &opts,
                &Changed {
                    opts: &opts,
                    change
                },
                &mut vec![]
            )
            .is_err());
            assert!(old(&opts).join("xshelld").exists());
        }
    }

    #[test]
    fn process_denial_truncation_disappearance_and_changed_identity_refuse() {
        enum Fault {
            Denied,
            Truncated,
            Disappeared,
            Changed,
        }
        struct ProcFault {
            fault: Fault,
            reads: Cell<usize>,
        }
        impl Io for ProcFault {
            fn process_entries(&self) -> Result<Vec<OsString>> {
                OneProcess.process_entries()
            }
            fn proc_read(&self, path: &Path, cap: usize) -> Result<Vec<u8>> {
                if path == Path::new(&format!("/proc/{}/stat", std::process::id())) {
                    self.reads.set(self.reads.get() + 1);
                    match self.fault {
                        Fault::Denied => {
                            return Err("injected process read permission denied".into())
                        }
                        Fault::Truncated => return Ok(b"truncated".to_vec()),
                        Fault::Disappeared => return Err("injected unresolved process exit".into()),
                        Fault::Changed if self.reads.get() > 1 => {
                            let bytes = Native.proc_read(path, cap)?;
                            let text = String::from_utf8(bytes).unwrap();
                            let (head, tail) = text.rsplit_once(") ").unwrap();
                            let mut fields: Vec<_> =
                                tail.split_whitespace().map(str::to_owned).collect();
                            fields[19] = "0".into();
                            return Ok(format!("{head}) {}", fields.join(" ")).into_bytes());
                        }
                        _ => {}
                    }
                }
                Native.proc_read(path, cap)
            }
        }
        for fault in [
            Fault::Denied,
            Fault::Truncated,
            Fault::Disappeared,
            Fault::Changed,
        ] {
            let (_temp, opts) = fixture();
            assert!(execute(
                &opts,
                &ProcFault {
                    fault,
                    reads: Cell::new(0)
                },
                &mut vec![]
            )
            .is_err());
            assert!(old(&opts).join("xshelld").exists());
        }
    }

    #[test]
    fn contradictory_visibility_and_restricted_proc_mounts_refuse() {
        struct VisibilityFault {
            path: &'static str,
            bytes: &'static [u8],
        }
        impl Io for VisibilityFault {
            fn proc_read(&self, path: &Path, cap: usize) -> Result<Vec<u8>> {
                if path == Path::new(self.path) {
                    Ok(self.bytes.to_vec())
                } else {
                    Native.proc_read(path, cap)
                }
            }
        }
        for (path, bytes) in [
            ("/proc/self/status", b"NSpid:\t1\t2\n".as_slice()),
            ("/proc/1/status", b"NSpid:\t2\n".as_slice()),
            ("/proc/self/uid_map", b"0 1000 1\n".as_slice()),
            (
                "/proc/self/mountinfo",
                b"1 1 0:1 / /proc rw - proc proc rw,hidepid=2\n".as_slice(),
            ),
            (
                "/proc/self/mountinfo",
                b"1 1 0:1 / /proc rw - proc proc rw,subset=pid\n".as_slice(),
            ),
        ] {
            let (_temp, opts) = fixture();
            assert!(execute(&opts, &VisibilityFault { path, bytes }, &mut vec![]).is_err());
            assert!(old(&opts).join("xshelld").exists());
        }
    }

    #[test]
    fn explicit_bounds_and_deadline_fail_closed() {
        let (_temp, opts) = fixture();
        let file = opts.home.join("large");
        fs::write(&file, b"12345").unwrap();
        assert!(bounded(File::open(file).unwrap(), 4).is_err());
        assert!(names(&File::open(&opts.home).unwrap(), 0).is_err());
        let mut budget = Budget::new();
        assert!(budget.charge(MAX_TOTAL + 1).is_err());
        let expired = Budget {
            start: Instant::now() - DEADLINE - Duration::from_secs(1),
            bytes: 0,
        };
        assert!(expired.check().is_err());
        struct ManyProcesses;
        impl Io for ManyProcesses {
            fn process_entries(&self) -> Result<Vec<OsString>> {
                Ok(vec![OsString::from("1"); MAX_PROCESSES + 257])
            }
        }
        assert!(execute(&opts, &ManyProcesses, &mut vec![])
            .unwrap_err()
            .contains("process entry bound"));
        assert!(old(&opts).join("xshelld").exists());
    }

    #[test]
    fn generated_hook_quotes_and_keyed_names_not_just_valid_names() {
        let exe = Path::new("/private/has ' quote/1.0.0+111111111111/xshelld");
        let bytes = xshell_core::agent_status::claude_settings_json(exe);
        assert_eq!(hook_executable(bytes.as_bytes()).unwrap(), exe);
        assert!(hook_executable(b"{}").is_err());
        let stamp = b"@@XSHELL-BUILD-V1:111111111111:1@@";
        assert!(keyed("1.0.0+111111111111", stamp));
        for name in [
            "1.0.0",
            "1.0.0+222222222222",
            "garbage+111111111111",
            "1.0.0+unknown",
        ] {
            assert!(!keyed(name, stamp));
        }
        assert!(!keyed("1.0.0+111111111111", b"@@XSHELL-BUILD-V1:unknown@@"));
    }
    #[test]
    fn pid1_ptrace_denial_is_not_same_uid_inventory_denial() {
        struct Pid1Denied;
        impl Io for Pid1Denied {
            fn pid1_namespace(&self) -> io::Result<PathBuf> {
                Err(io::Error::from_raw_os_error(libc::EACCES))
            }
            fn process_entries(&self) -> Result<Vec<OsString>> {
                OneProcess.process_entries()
            }
        }
        let (_temp, opts) = fixture();
        execute(&opts, &Pid1Denied, &mut vec![]).unwrap();
        assert!(!old(&opts).exists());
        struct WrongNamespace;
        impl Io for WrongNamespace {
            fn pid1_namespace(&self) -> io::Result<PathBuf> {
                Ok(PathBuf::from("pid:[contradictory]"))
            }
        }
        let (_temp, opts) = fixture();
        assert!(execute(&opts, &WrongNamespace, &mut vec![])
            .unwrap_err()
            .contains("inconsistent PID namespace"));
        assert!(old(&opts).join("xshelld").exists());
    }

    #[test]
    fn generated_codex_pi_and_shell_references_are_observed_not_cached_path_detection() {
        let target = Path::new("/private/home/.xshell/server");
        let hooks = xshell_core::agent_status::AgentHooks {
            exe: target.join("1.0.0+111111111111/xshelld"),
            endpoint: "/private/event.sock".into(),
            claude_settings: PathBuf::from("/other-home/.xshell/daemon/claude-hooks.json"),
            pi_extension: None,
        };
        let argv = hooks.codex_overrides().join("\0").into_bytes();
        assert!(product_reference(
            Path::new("/usr/bin/codex"),
            &argv,
            b"",
            target
        ));
        assert!(product_reference(
            Path::new("/usr/bin/pi"),
            b"pi\0",
            format!("XSHELL_EVENT_EXE={}\0", hooks.exe.display()).as_bytes(),
            target
        ));
        assert!(product_reference(
            Path::new("/bin/sh"),
            hooks.ended_command_posix().as_bytes(),
            b"",
            target
        ));
        assert!(!product_reference(
            Path::new("/bin/sh"),
            b"sh\0",
            b"",
            target
        ));
        // This last process is still blocked by the unknown-process rule, not declared safe.
    }

    #[test]
    fn candidate_mount_exdev_cannot_unlink_outside_alias() {
        struct MountedCandidate {
            unlinks: Cell<usize>,
        }
        impl Io for MountedCandidate {
            fn beneath(&self, dir: RawFd, path: &OsStr, directory: bool) -> io::Result<File> {
                if path != "." {
                    Err(io::Error::from_raw_os_error(libc::EXDEV))
                } else {
                    Native.beneath(dir, path, directory)
                }
            }
            fn unlink(&self, _: RawFd, _: &CStr, _: i32) -> io::Result<()> {
                self.unlinks.set(self.unlinks.get() + 1);
                panic!("no outside-alias unlink")
            }
        }
        let (_temp, opts) = fixture();
        let outside = opts.home.join("outside-xshelld");
        fs::write(&outside, b"outside executable/sentinel").unwrap();
        let before: Identity = fs::metadata(&outside).unwrap().into();
        let io = MountedCandidate {
            unlinks: Cell::new(0),
        };
        assert!(execute(&opts, &io, &mut vec![])
            .unwrap_err()
            .contains("candidate mount boundary"));
        assert_eq!(io.unlinks.get(), 0);
        assert_eq!(Identity::from(fs::metadata(&outside).unwrap()), before);
        assert_eq!(fs::read(outside).unwrap(), b"outside executable/sentinel");
        assert!(old(&opts).join("xshelld").exists());
    }
}
