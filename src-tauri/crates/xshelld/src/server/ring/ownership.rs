//! Lifetime exclusion for one canonical Store, separate from short `ring.lock` writes.
use super::Store;
use std::fs;
use std::io;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

pub(crate) struct StoreOwnershipLease(fs::File);
pub(crate) enum StoreOwnership {
    Acquired(StoreOwnershipLease),
    Busy,
}
impl Drop for StoreOwnershipLease {
    fn drop(&mut self) {
        // Only the final admitted reference drops this object. Forked children must not
        // keep the parent's open-file-description lock alive after that point.
        let _ = self.0.unlock();
    }
}

/// Resolve only the home alias; private components must themselves be owned real dirs.
pub(crate) fn canonical_store(directory: &Path) -> io::Result<PathBuf> {
    let home = directory
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .ok_or_else(|| io::Error::other("invalid Daemon Store association"))?
        .canonicalize()?;
    #[cfg(unix)]
    {
        prepare_store(&home)
    }
    #[cfg(windows)]
    {
        xshell_core::private_fs::check_owned_dir(&home)?;
        let mut dir = home;
        for component in [".xshell", "daemon", "ring"] {
            dir.push(component);
            xshell_core::private_fs::ensure_dir(&dir)?;
        }
        Ok(dir)
    }
}

#[cfg(unix)]
fn prepare_store(home: &Path) -> io::Result<PathBuf> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let first = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(home)?;
    let mut chain = vec![(home.to_path_buf(), first)];
    validate_setup_chain(&chain)?;
    for component in [".xshell", "daemon", "ring"] {
        let (parent, file) = chain.last().unwrap();
        let path = parent.join(component);
        let name = CString::new(component).unwrap();
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        let mut fd = unsafe { libc::openat(file.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::NotFound {
                return Err(error);
            }
            // Every existing component has been checked before creating a missing child.
            validate_setup_chain(&chain)?;
            if unsafe { libc::mkdirat(file.as_raw_fd(), name.as_ptr(), 0o700) } < 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::AlreadyExists {
                    return Err(error);
                }
            }
            fd = unsafe { libc::openat(file.as_raw_fd(), name.as_ptr(), flags) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        chain.push((path, unsafe { fs::File::from_raw_fd(fd) }));
        validate_setup_chain(&chain)?;
    }
    // No chmod until the entire existing namespace and all opened identities are safe.
    for (_, file) in chain.iter().skip(2) {
        validate_setup_chain(&chain)?;
        if file.metadata()?.mode() & 0o7777 != 0o700 {
            file.set_permissions(fs::Permissions::from_mode(0o700))?;
        }
    }
    validate_setup_chain(&chain)?;
    let dir = &chain.last().unwrap().0;
    validate_store(dir)?;
    Ok(dir.clone())
}

#[cfg(unix)]
fn validate_setup_chain(chain: &[(PathBuf, fs::File)]) -> io::Result<()> {
    for (path, file) in chain {
        let opened = file.metadata()?;
        let named = fs::symlink_metadata(path)?;
        if !opened.is_dir()
            || !named.is_dir()
            || opened.uid() != unsafe { libc::geteuid() }
            || opened.mode() & 0o022 != 0
            || named.uid() != unsafe { libc::geteuid() }
            || named.mode() & 0o022 != 0
            || (opened.dev(), opened.ino()) != (named.dev(), named.ino())
            || path.canonicalize()? != *path
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unsafe or changed Daemon Store setup directory",
            ));
        }
    }
    Ok(())
}

/// Post-election validation must never repair a broadened or replaced namespace.
#[cfg(unix)]
fn validate_store(dir: &Path) -> io::Result<()> {
    for (index, path) in dir.ancestors().take(4).enumerate() {
        if index < 2 {
            xshell_core::local_ipc::private_dir(path)?;
        } else {
            xshell_core::companion_reservation::validate_shared_ancestor(path)?;
        }
    }
    Ok(())
}
impl StoreOwnershipLease {
    pub(crate) fn acquire(directory: &Path) -> Result<StoreOwnership, String> {
        let dir = canonical_store(directory).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        let ancestry = ancestry_identity(&dir).map_err(|e| e.to_string())?;
        let store = Store::new(dir.clone());
        let file = store.open_lock("runtime-owner.lock")?;
        #[cfg(unix)]
        let before = file.metadata().map_err(|e| e.to_string())?;
        let acquired = match file.try_lock() {
            Ok(()) => true,
            Err(fs::TryLockError::WouldBlock) => false,
            Err(fs::TryLockError::Error(e)) => return Err(e.to_string()),
        };
        // Validate even Busy: a replaced/unsafe namespace is not a live-owner route.
        #[cfg(windows)]
        if canonical_store(&dir).map_err(|e| e.to_string())? != dir {
            return Err("Daemon Store changed during ownership acquisition".into());
        }
        #[cfg(unix)]
        {
            validate_store(&dir).map_err(|e| e.to_string())?;
            if ancestry_identity(&dir).map_err(|e| e.to_string())? != ancestry {
                return Err("Daemon Store ancestry changed during acquisition".into());
            }
            let after =
                fs::symlink_metadata(dir.join("runtime-owner.lock")).map_err(|e| e.to_string())?;
            if !after.is_file()
                || after.uid() != unsafe { libc::geteuid() }
                || before.dev() != after.dev()
                || before.ino() != after.ino()
            {
                return Err("Daemon ownership lock changed during acquisition".into());
            }
        }
        #[cfg(windows)]
        {
            let reopened = xshell_core::private_fs::open_read(&dir.join("runtime-owner.lock"))
                .map_err(|e| e.to_string())?
                .ok_or("Daemon ownership lock disappeared")?;
            if windows_file_identity(&file).map_err(|e| e.to_string())?
                != windows_file_identity(&reopened).map_err(|e| e.to_string())?
            {
                return Err("Daemon ownership lock changed during acquisition".into());
            }
        }
        Ok(if acquired {
            StoreOwnership::Acquired(Self(file))
        } else {
            StoreOwnership::Busy
        })
    }
}

#[cfg(unix)]
fn ancestry_identity(dir: &Path) -> io::Result<Vec<(u64, u64)>> {
    dir.ancestors()
        .take(4)
        .map(|path| {
            let meta = fs::symlink_metadata(path)?;
            Ok((meta.dev(), meta.ino()))
        })
        .collect()
}
#[cfg(windows)]
fn windows_file_identity(file: &fs::File) -> io::Result<(u32, u32, u32)> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((
        info.dwVolumeSerialNumber,
        info.nFileIndexHigh,
        info.nFileIndexLow,
    ))
}

struct Slot {
    lease: Option<Arc<StoreOwnershipLease>>,
    closed: bool,
    standalone_cancel: Option<Weak<AtomicBool>>,
}
/// Short admission mutex only; the returned operation, not this mutex, spans work.
pub(crate) struct StoreAdmission(Mutex<Slot>);
pub(crate) struct StoreOperation {
    _lease: Arc<StoreOwnershipLease>,
    admission: Weak<StoreAdmission>,
    cancel: Option<Weak<AtomicBool>>,
}
impl StoreAdmission {
    pub(crate) fn new(lease: StoreOwnershipLease) -> Arc<Self> {
        Arc::new(Self(Mutex::new(Slot {
            lease: Some(Arc::new(lease)),
            closed: false,
            standalone_cancel: None,
        })))
    }
    pub(crate) fn admit(self: &Arc<Self>) -> Result<StoreOperation, String> {
        self.admit_with_cancel(None)
    }
    #[cfg(unix)]
    pub(crate) fn admit_standalone(
        self: &Arc<Self>,
        cancel: &Arc<AtomicBool>,
    ) -> Result<StoreOperation, String> {
        self.admit_with_cancel(Some(cancel))
    }
    fn admit_with_cancel(
        self: &Arc<Self>,
        cancel: Option<&Arc<AtomicBool>>,
    ) -> Result<StoreOperation, String> {
        let mut slot = self.0.lock().unwrap();
        if slot.closed {
            return Err("Daemon Store admission is closed".into());
        }
        if cancel.is_some() && slot.standalone_cancel.is_some() {
            return Err("standalone command already admitted".into());
        }
        let lease = slot
            .lease
            .as_ref()
            .ok_or("Daemon Store ownership released")?
            .clone();
        let cancel = cancel.map(Arc::downgrade);
        if cancel.is_some() {
            slot.standalone_cancel = cancel.clone();
        }
        Ok(StoreOperation {
            _lease: lease,
            admission: Arc::downgrade(self),
            cancel,
        })
    }
    pub(crate) fn close(&self, exiting: &AtomicBool) -> bool {
        let cancel = {
            let mut slot = self.0.lock().unwrap();
            if slot.closed {
                return false;
            }
            slot.closed = true;
            exiting.store(true, Ordering::SeqCst);
            slot.standalone_cancel.take().and_then(|c| c.upgrade())
        };
        if let Some(cancel) = cancel {
            cancel.store(true, Ordering::Release);
        }
        true
    }
    pub(crate) fn release(&self) {
        let lease = self.0.lock().unwrap().lease.take();
        drop(lease);
    }
}
impl Drop for StoreOperation {
    fn drop(&mut self) {
        if let (Some(admission), Some(cancel)) = (self.admission.upgrade(), &self.cancel) {
            let mut slot = admission.0.lock().unwrap();
            if slot
                .standalone_cancel
                .as_ref()
                .is_some_and(|c| c.ptr_eq(cancel))
            {
                slot.standalone_cancel = None;
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::server::{Config, Server, StartError, TestHook, TestPoint};
    use std::os::unix::fs::symlink;
    use std::sync::mpsc;
    use std::time::Duration;

    fn directory(home: &Path) -> PathBuf {
        home.join(".xshell/daemon/ring")
    }
    fn acquired(dir: &Path) -> StoreOwnershipLease {
        match StoreOwnershipLease::acquire(dir).unwrap() {
            StoreOwnership::Acquired(lease) => lease,
            StoreOwnership::Busy => panic!("unexpected Store owner"),
        }
    }
    #[test]
    fn canonical_store_preserves_safe_shared_ancestor_and_existing_store_bytes() {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let home = tempfile::tempdir().unwrap();
        let shared = home.path().join(".xshell");
        fs::create_dir(&shared).unwrap();
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o755)).unwrap();
        let before = fs::symlink_metadata(&shared).unwrap();
        let dir = directory(home.path());
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .unwrap();
        let retained = ["keys.json", "roster.json", "standalone-adoption.json"];
        for name in retained {
            fs::write(dir.join(name), format!("opaque retained {name}")).unwrap();
            fs::set_permissions(dir.join(name), fs::Permissions::from_mode(0o600)).unwrap();
        }
        fs::write(shared.join("existing-install"), b"opaque installed bytes").unwrap();
        let _lease = acquired(&dir);
        let after = fs::symlink_metadata(&shared).unwrap();
        assert_eq!((before.dev(), before.ino()), (after.dev(), after.ino()));
        assert_eq!(after.mode() & 0o777, 0o755);
        for path in [shared.join("daemon"), dir.clone()] {
            assert_eq!(fs::metadata(path).unwrap().mode() & 0o777, 0o700);
        }
        assert_eq!(
            fs::metadata(dir.join("runtime-owner.lock")).unwrap().mode() & 0o777,
            0o600
        );
        for name in retained {
            assert_eq!(
                fs::read(dir.join(name)).unwrap(),
                format!("opaque retained {name}").as_bytes()
            );
        }
        assert_eq!(
            fs::read(shared.join("existing-install")).unwrap(),
            b"opaque installed bytes"
        );
    }
    #[test]
    fn canonical_store_tightens_each_readable_private_directory_without_replacing_data() {
        use std::os::unix::fs::PermissionsExt;
        for component in [".xshell/daemon", ".xshell/daemon/ring"] {
            let home = tempfile::tempdir().unwrap();
            let dir = canonical_store(&directory(home.path())).unwrap();
            let shared = home.path().join(".xshell");
            for path in [home.path(), &shared] {
                fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
            }
            let paths = [
                home.path().to_path_buf(),
                shared,
                dir.parent().unwrap().into(),
                dir.clone(),
            ];
            let before: Vec<_> = paths
                .iter()
                .map(|p| fs::symlink_metadata(p).unwrap())
                .collect();
            let readable = home.path().join(component);
            fs::set_permissions(&readable, fs::Permissions::from_mode(0o755)).unwrap();
            let retained = dir.join("opaque-store-data");
            fs::write(&retained, b"retained, not identity authority").unwrap();
            let data_before = fs::symlink_metadata(&retained).unwrap();
            let _lease = acquired(&dir);
            for (index, (path, before)) in paths.iter().zip(before).enumerate() {
                let after = fs::symlink_metadata(path).unwrap();
                assert_eq!((before.dev(), before.ino()), (after.dev(), after.ino()));
                assert_eq!(after.mode() & 0o777, if index < 2 { 0o755 } else { 0o700 });
            }
            let data_after = fs::symlink_metadata(&retained).unwrap();
            assert_eq!(
                (data_before.dev(), data_before.ino()),
                (data_after.dev(), data_after.ino())
            );
            assert_eq!(data_before.mode(), data_after.mode());
            assert_eq!(
                fs::read(retained).unwrap(),
                b"retained, not identity authority"
            );
            assert_eq!(
                fs::metadata(dir.join("runtime-owner.lock")).unwrap().mode() & 0o777,
                0o600
            );
        }
    }
    #[test]
    fn canonical_store_unsafe_later_component_does_not_tighten_earlier_directory() {
        use std::os::unix::fs::PermissionsExt;
        for kind in 0..4 {
            let home = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            fs::set_permissions(outside.path(), fs::Permissions::from_mode(0o755)).unwrap();
            fs::write(outside.path().join("retained"), b"outside bytes").unwrap();
            let outside_before = fs::symlink_metadata(outside.path()).unwrap();
            let dir = canonical_store(&directory(home.path())).unwrap();
            let daemon = dir.parent().unwrap();
            fs::set_permissions(daemon, fs::Permissions::from_mode(0o755)).unwrap();
            let before = fs::symlink_metadata(daemon).unwrap();
            match kind {
                0 => {
                    fs::remove_dir(&dir).unwrap();
                    symlink(outside.path(), &dir).unwrap();
                }
                1 => {
                    fs::remove_dir(&dir).unwrap();
                    fs::write(&dir, b"non-directory bytes").unwrap();
                }
                _ => fs::set_permissions(
                    &dir,
                    fs::Permissions::from_mode(if kind == 2 { 0o775 } else { 0o777 }),
                )
                .unwrap(),
            }
            let unsafe_before = fs::symlink_metadata(&dir).unwrap();
            assert!(StoreOwnershipLease::acquire(&dir).is_err());
            let after = fs::symlink_metadata(daemon).unwrap();
            assert_eq!(
                (before.dev(), before.ino(), before.mode()),
                (after.dev(), after.ino(), after.mode())
            );
            let unsafe_after = fs::symlink_metadata(&dir).unwrap();
            assert_eq!(
                (
                    unsafe_before.dev(),
                    unsafe_before.ino(),
                    unsafe_before.mode()
                ),
                (unsafe_after.dev(), unsafe_after.ino(), unsafe_after.mode())
            );
            let outside_after = fs::symlink_metadata(outside.path()).unwrap();
            assert_eq!(
                (
                    outside_before.dev(),
                    outside_before.ino(),
                    outside_before.mode()
                ),
                (
                    outside_after.dev(),
                    outside_after.ino(),
                    outside_after.mode()
                )
            );
            assert_eq!(
                fs::read(outside.path().join("retained")).unwrap(),
                b"outside bytes"
            );
            assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 1);
            if kind == 1 {
                assert_eq!(fs::read(&dir).unwrap(), b"non-directory bytes");
            }
            assert!(!dir.join("runtime-owner.lock").exists());
        }
    }
    #[test]
    fn ownership_post_lock_validation_refuses_readable_private_directory_without_repair() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let dir = directory(home.path());
        let _lease = acquired(&dir);
        for path in [dir.parent().unwrap(), dir.as_path()] {
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
            assert!(validate_store(&dir).is_err());
            assert_eq!(fs::metadata(path).unwrap().mode() & 0o777, 0o755);
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
    }
    #[test]
    fn canonical_store_home_aliases_contend_and_separate_stores_do_not() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        fs::create_dir(&home).unwrap();
        let alias = root.path().join("alias");
        symlink(&home, &alias).unwrap();
        let lease = acquired(&directory(&home));
        assert!(matches!(
            StoreOwnershipLease::acquire(&directory(&alias)).unwrap(),
            StoreOwnership::Busy
        ));
        let other = tempfile::tempdir().unwrap();
        let _other = acquired(&directory(other.path()));
        drop(lease);
        let _successor = acquired(&directory(&alias));
    }
    #[test]
    fn canonical_store_rejects_private_directory_and_lock_symlinks_nonregular_and_unsafe_ancestry()
    {
        use std::os::unix::fs::PermissionsExt;
        for kind in 0..4 {
            let home = tempfile::tempdir().unwrap();
            let dir = canonical_store(&directory(home.path())).unwrap();
            let lock = dir.join("runtime-owner.lock");
            match kind {
                0 => {
                    symlink(home.path().join("untouched"), &lock).unwrap();
                }
                1 => {
                    fs::create_dir(&lock).unwrap();
                }
                2 => {
                    fs::remove_dir(&dir).unwrap();
                    symlink(home.path(), &dir).unwrap();
                }
                _ => {
                    fs::set_permissions(
                        home.path().join(".xshell"),
                        fs::Permissions::from_mode(0o775),
                    )
                    .unwrap();
                }
            }
            assert!(StoreOwnershipLease::acquire(&dir).is_err());
            assert!(!dir.join("keys.json").exists());
            assert!(!dir.join("roster.json").exists());
            assert!(!dir.join("standalone-adoption.json").exists());
            assert!(!home.path().join("untouched").exists());
        }
    }
    #[test]
    fn canonical_store_rejects_writable_home_shared_ancestor_and_private_leaves() {
        use std::os::unix::fs::PermissionsExt;
        for (component, mode) in [
            ("", 0o775),
            ("", 0o777),
            (".xshell", 0o777),
            (".xshell/daemon", 0o775),
            (".xshell/daemon", 0o777),
            (".xshell/daemon/ring", 0o775),
            (".xshell/daemon/ring", 0o777),
        ] {
            let home = tempfile::tempdir().unwrap();
            let dir = canonical_store(&directory(home.path())).unwrap();
            let journal = dir.join("standalone-adoption.json");
            fs::write(&journal, b"opaque retained journal").unwrap();
            let unsafe_path = home.path().join(component);
            fs::set_permissions(&unsafe_path, fs::Permissions::from_mode(mode)).unwrap();
            assert!(StoreOwnershipLease::acquire(&dir).is_err());
            assert_eq!(fs::metadata(unsafe_path).unwrap().mode() & 0o777, mode);
            assert_eq!(fs::read(journal).unwrap(), b"opaque retained journal");
            assert!(!dir.join("runtime-owner.lock").exists());
            assert!(!dir.join("keys.json").exists());
        }
        let foreign = Path::new("/");
        if fs::metadata(foreign).unwrap().uid() != unsafe { libc::geteuid() } {
            assert!(StoreOwnershipLease::acquire(&directory(foreign)).is_err());
        }
    }
    #[test]
    fn canonical_store_rejects_symlink_or_nonregular_shared_ancestor_without_mutation() {
        for link in [false, true] {
            let home = tempfile::tempdir().unwrap();
            let shared = home.path().join(".xshell");
            let outside = tempfile::tempdir().unwrap();
            if link {
                symlink(outside.path(), &shared).unwrap();
            } else {
                fs::write(&shared, b"not a directory").unwrap();
            }
            assert!(StoreOwnershipLease::acquire(&directory(home.path())).is_err());
            assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
            if !link {
                assert_eq!(fs::read(shared).unwrap(), b"not a directory");
            }
        }
    }
    #[test]
    fn store_owner_precedes_endpoint_election_across_custom_and_xdg_paths() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().canonicalize().unwrap();
        let paths = crate::paths::resolve(&root, None, Some(&root.join("first/daemon.sock")));
        let d = Server::start(Config::new(root.clone(), paths.clone())).unwrap();
        for other in [
            crate::paths::resolve(&root, None, Some(&root.join("second/daemon.sock"))),
            crate::paths::resolve(&root, Some(&root.join("xdg")), None),
        ] {
            assert_ne!(other.lock, paths.lock);
            assert!(matches!(
                Server::start(Config::new(root.clone(), other.clone())),
                Err(StartError::AlreadyRunning)
            ));
            assert!(!other.lock.exists());
        }
        assert!(!paths.ring_dir.join("keys.json").exists());
        d.shutdown();
        let _successor = acquired(&paths.ring_dir);
    }
    #[test]
    fn admitted_operation_retains_lease_after_base_release_and_cancels_live_driver() {
        let home = tempfile::tempdir().unwrap();
        let dir = directory(home.path());
        let admission = StoreAdmission::new(acquired(&dir));
        let cancel = Arc::new(AtomicBool::new(false));
        let operation = admission.admit_standalone(&cancel).unwrap();
        let exiting = AtomicBool::new(false);
        assert!(admission.close(&exiting));
        assert!(cancel.load(Ordering::Acquire));
        assert!(exiting.load(Ordering::Acquire));
        assert!(admission.admit().is_err());
        admission.release();
        assert!(matches!(
            StoreOwnershipLease::acquire(&dir).unwrap(),
            StoreOwnership::Busy
        ));
        drop(operation);
        let _successor = acquired(&dir);
    }
    #[test]
    fn failed_and_aborted_startup_release_store_without_creating_keys() {
        let home = tempfile::tempdir().unwrap();
        let paths = crate::paths::resolve(home.path(), None, None);
        crate::paths::ensure_private_dir(paths.mode.parent().unwrap()).unwrap();
        fs::create_dir(&paths.mode).unwrap();
        assert!(Server::start(Config::new(home.path().into(), paths.clone())).is_err());
        drop(acquired(&paths.ring_dir));
        fs::remove_dir(&paths.mode).unwrap();
        let mut cfg = Config::new(home.path().into(), paths.clone());
        let abort = Arc::new(crate::server::StopLatch::default());
        abort.trigger();
        cfg.abort = Some(abort);
        assert!(matches!(Server::start(cfg), Err(StartError::Aborted)));
        let _successor = acquired(&paths.ring_dir);
        assert!(!paths.ring_dir.join("keys.json").exists());
    }
    #[test]
    fn admitted_roster_callback_retains_store_until_write_and_late_callback_cannot_write() {
        use xshell_protocol::ring::{DeviceKeys, RosterChain, SignedRoster};
        let home = tempfile::tempdir().unwrap();
        let paths = crate::paths::resolve(home.path(), None, None);
        let (at, reached) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let released = Mutex::new(released);
        let mut cfg = Config::new(home.path().into(), paths.clone());
        cfg.test_hook = Some(TestHook(Arc::new(move |_, point| {
            if point == TestPoint::RosterCallbackAdmitted {
                at.send(()).unwrap();
                released
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap();
            }
            false
        })));
        let handle = Server::start(cfg).unwrap();
        let keys = DeviceKeys::generate().unwrap();
        // Signed v1 persistence control only; not v2 enrollment/socket-delivery evidence.
        let genesis = SignedRoster::genesis(
            &keys,
            keys.noise_key(),
            "callback storage",
            "ws://127.0.0.1:9",
            1,
        )
        .unwrap();
        let chain = RosterChain::from_chain(vec![genesis]).unwrap();
        let sessions = super::super::Sessions::new(
            xshell_protocol::ring::relay::sessions::SessionsConfig::new(Arc::new(
                DeviceKeys::generate().unwrap(),
            )),
            None,
        );
        std::thread::scope(|scope| {
            let callback =
                scope.spawn(|| handle.d.ring.persist.keep_newer(&chain, None, &sessions));
            reached.recv_timeout(Duration::from_secs(5)).unwrap();
            handle.d.exit(crate::server::ExitReason::Shutdown);
            assert!(matches!(
                StoreOwnershipLease::acquire(&paths.ring_dir).unwrap(),
                StoreOwnership::Busy
            ));
            assert!(!paths.ring_dir.join("roster.json").exists());
            release.send(()).unwrap();
            callback.join().unwrap();
        });
        let before = fs::read(paths.ring_dir.join("roster.json")).unwrap();
        let _successor = acquired(&paths.ring_dir);
        let next = chain.head().next(&keys, 2, |_| {}).unwrap();
        let next = chain.extended(&[next]).unwrap().0;
        handle.d.ring.persist.keep_newer(&next, None, &sessions);
        assert_eq!(
            fs::read(paths.ring_dir.join("roster.json")).unwrap(),
            before
        );
    }
}
