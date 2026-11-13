//! Deny-only startup metadata. This boundary never reads keys or grants registration,
//! signing, membership or consent authority. Daemon registration may clear only a
//! non-uncertain reservation; it cannot resolve an incumbent legacy remote outcome.
#![cfg(unix)]
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

// Policy preparation carries at most the existing 17MiB companion envelope, plus one
// accepted roster token. Profile paths/names may need six-byte JSON escapes; policy
// retirement is bounded to64 entries by CompanionPolicy::start. Header space includes
// the repeated intent/identity, proof pair, phase/possession and JSON field framing.
pub const MAX_COMPANION_POLICY_JSON_BYTES: usize =
    xshell_protocol::ring::companion::MAX_COMPANION_FRAME
        .checked_add(xshell_protocol::ring::roster::MAX_ROSTER_TOKEN)
        .expect("policy token bound")
        .checked_add(
            6usize
                .checked_mul(
                    4096usize
                        .checked_mul(2)
                        .expect("profile stores")
                        .checked_add(1024)
                        .expect("profile endpoint")
                        .checked_add(128)
                        .expect("identity names"),
                )
                .expect("profile escapes"),
        )
        .expect("profile JSON bound")
        .checked_add(
            64usize
                .checked_mul(43 + 20 + 8)
                .expect("retired operations"),
        )
        .expect("retired JSON bound")
        .checked_add(8192)
        .expect("policy metadata framing");
pub const FILE: &str = "companion-pending.json";
/// Sticky denial only: it cannot attest registration or resolve a legacy remote outcome.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CompanionReservation {
    pub desktop_store: String,
    #[serde(default)]
    pub legacy_outcome_uncertain: bool,
}
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum ReservationFile {
    Current(CompanionReservation),
    Legacy(String),
}
pub fn read_reservation(directory: &Path) -> io::Result<Option<CompanionReservation>> {
    read_metadata(&directory.join(FILE))?
        .map(
            |bytes| match serde_json::from_slice(&bytes).map_err(io::Error::other)? {
                ReservationFile::Current(reservation) => Ok(reservation),
                ReservationFile::Legacy(desktop_store) => Ok(CompanionReservation {
                    desktop_store,
                    legacy_outcome_uncertain: false,
                }),
            },
        )
        .transpose()
}
fn denied(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}
fn owned_directory(path: &Path, private: bool) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } || path.canonicalize()? != path {
        return Err(denied("ambiguous or foreign counterpart directory"));
    }
    if private && meta.mode() & 0o077 != 0 {
        return Err(denied("counterpart directory is not private"));
    }
    Ok(())
}
/// CLI classification must reject an unsafe local store even when no marker could be
/// reserved there. This is a path-safety check, not permission to discard a reservation.
pub fn validate_daemon_store(directory: &Path) -> io::Result<()> {
    let home = directory
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .ok_or_else(|| denied("invalid local Daemon store association"))?
        .canonicalize()?;
    owned_directory(&home, false)?;
    let mut path = home;
    for component in [".xshell", "daemon", "ring"] {
        path.push(component);
        match fs::symlink_metadata(&path) {
            Ok(_) => owned_directory(&path, true)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
/// Trusted local setup supplies the actual home and Desktop Store, never a web/Host path.
/// Does not load/create Daemon keys, a policy, membership, or an endpoint.
pub fn reserve(home: &Path, desktop_store: &Path) -> io::Result<PathBuf> {
    reserve_with_sync(home, desktop_store, sync_directory)
}
fn sync_directory(path: &Path) -> io::Result<()> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } {
        return Err(denied("unsafe directory sync target"));
    }
    file.sync_all()
}
fn prepare_directory(
    home: &Path,
    desktop_store: &Path,
    sync: &mut impl FnMut(&Path) -> io::Result<()>,
) -> io::Result<PathBuf> {
    owned_directory(home, false)?;
    owned_directory(desktop_store, true)?;
    let mut dir = home.to_path_buf();
    for component in [".xshell", "daemon", "ring"] {
        let parent = dir.clone();
        dir.push(component);
        match fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        owned_directory(&dir, true)?;
        // Sync existing entries too: a previous attempt may have failed its parent sync.
        sync(&parent)?;
    }
    if dir == desktop_store {
        return Err(denied("counterpart stores must be distinct"));
    }
    Ok(dir)
}
fn reserve_with_sync(
    home: &Path,
    desktop_store: &Path,
    mut sync: impl FnMut(&Path) -> io::Result<()>,
) -> io::Result<PathBuf> {
    let dir = prepare_directory(home, desktop_store, &mut sync)?;
    let _ring = lock(&dir.join("ring.lock"), true)?;
    reserve_locked_inner(&dir, desktop_store, &mut sync)?;
    Ok(dir)
}
/// Daemon-side typed reservation uses this same denial boundary while holding ring.lock.
/// No offer lease is needed to persist denial; a live incumbent makes that denial sticky.
pub fn reserve_locked(directory: &Path, desktop_store: &Path) -> io::Result<()> {
    let home = directory
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .ok_or_else(|| denied("invalid local Daemon store association"))?;
    let mut sync = sync_directory;
    if prepare_directory(home, desktop_store, &mut sync)? != directory {
        return Err(denied("local Daemon store association mismatch"));
    }
    reserve_locked_inner(directory, desktop_store, &mut sync)
}
fn reserve_locked_inner(
    dir: &Path,
    desktop_store: &Path,
    sync: &mut impl FnMut(&Path) -> io::Result<()>,
) -> io::Result<()> {
    let desktop = desktop_store
        .to_str()
        .ok_or_else(|| denied("Desktop store path is not UTF-8"))?;
    let mut reservation = match read_reservation(dir)? {
        Some(existing) if existing.desktop_store != desktop => {
            return Err(denied("conflicting pending counterpart association"))
        }
        Some(existing) => existing,
        None => CompanionReservation {
            desktop_store: desktop.into(),
            legacy_outcome_uncertain: false,
        },
    };
    // Keep an acquired lease until the marker is durable, preventing a new legacy offer
    // slipping between absence detection and the marker. A busy lease never blocks denial.
    let _offer = match lock(&dir.join("ring-legacy-offer.lock"), false) {
        Ok(offer) => Some(offer),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
            // The old CLI persisted no pre-proof provenance. Do not infer that killing or
            // expiry revoked anything it might already have transmitted.
            reservation.legacy_outcome_uncertain = true;
            None
        }
        Err(error) => return Err(error),
    };
    if let Some(bytes) = read_companion_policy_metadata(&dir.join("companion.json"))? {
        let policy: xshell_protocol::ring::companion::CompanionPolicy =
            serde_json::from_slice(&bytes).map_err(io::Error::other)?;
        let profile = policy.profile();
        profile.validate().map_err(io::Error::other)?;
        if profile.desktop_store != desktop || Path::new(&profile.daemon_store) != dir {
            return Err(denied("conflicting retained counterpart association"));
        }
        if !reservation.legacy_outcome_uncertain {
            return Ok(());
        }
    }
    write_metadata(&dir.join(FILE), &serde_json::to_vec(&reservation)?)?;
    sync(dir)
}
fn lock(path: &Path, wait: bool) -> io::Result<fs::File> {
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.uid() != unsafe { libc::geteuid() } {
        return Err(denied("unsafe counterpart lock"));
    }
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(fs::TryLockError::WouldBlock) if wait && Instant::now() < until => {
                std::thread::sleep(Duration::from_millis(10))
            }
            Err(fs::TryLockError::WouldBlock) => {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "legacy offer active",
                ))
            }
            Err(fs::TryLockError::Error(error)) => return Err(error),
        }
    }
}
/// Public association/policy metadata only. Validate the opened object before bounded IO.
pub fn read_metadata(path: &Path) -> io::Result<Option<Vec<u8>>> {
    read_metadata_bounded(path, 1024 * 1024)
}
/// Unlike the small deny reservation, valid policy contains a prepared predecessor.
/// Shared with the Daemon's object-specific reader to keep both storage views compatible.
pub fn read_companion_policy_metadata(path: &Path) -> io::Result<Option<Vec<u8>>> {
    read_metadata_bounded(path, MAX_COMPANION_POLICY_JSON_BYTES)
}
fn read_metadata_bounded(path: &Path, maximum: usize) -> io::Result<Option<Vec<u8>>> {
    use std::io::Read;
    let file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let meta = file.metadata()?;
    if !meta.is_file()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.mode() & 0o077 != 0
        || meta.len() > maximum as u64
    {
        return Err(denied("unsafe counterpart metadata"));
    }
    let mut bytes = Vec::new();
    file.take(maximum as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > maximum {
        return Err(denied("counterpart metadata too large"));
    }
    Ok(Some(bytes))
}
static SEQ: AtomicU64 = AtomicU64::new(0);
fn write_metadata(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let temp = path.with_extension(format!(
        "{}.{}.tmp",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Prepared-policy storage compatibility only: no possession/consent or installation.
    #[test]
    fn valid_prepared_policy_with_embedded_history_exceeds_small_metadata_limit() {
        use xshell_protocol::ring::companion::{
            CompanionPolicy, CompanionProfile, CompanionProposal, ConsentTranscript,
        };
        use xshell_protocol::ring::membership::{
            Enrollment, EnrollmentKind, Membership, RingPolicy, RosterChange,
        };
        use xshell_protocol::ring::pairing::adoption::{AdoptionIntent, PublicIdentity};
        use xshell_protocol::ring::{b64, DeviceKeys, Member, Role, RosterChain, SignedRoster};
        let root = tempfile::tempdir().unwrap();
        let home = root.path().canonicalize().unwrap();
        let desktop_store = home.join("desktop");
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&desktop_store)
            .unwrap();
        let daemon_store = reserve(&home, &desktop_store).unwrap();
        let desktop = DeviceKeys::generate().unwrap();
        let daemon = DeviceKeys::generate().unwrap();
        let mut versions = vec![SignedRoster::genesis(
            &desktop,
            desktop.noise_key(),
            "retained",
            "ws://127.0.0.1:9",
            100,
        )
        .unwrap()];
        for _ in 0..32 {
            let next = versions
                .last()
                .unwrap()
                .next(&desktop, 100, |draft| {
                    draft.extra.insert(
                        "historicalStoragePadding".into(),
                        serde_json::Value::String("x".repeat(40 * 1024)),
                    );
                })
                .unwrap();
            versions.push(next);
        }
        let previous = versions.last().unwrap();
        let old_id = b64::encode(&[1; 32]);
        let mut upgrade = previous.roster().clone();
        upgrade.v = 2;
        upgrade.version += 1;
        upgrade.prev = Some(previous.hash());
        upgrade.extra.clear();
        upgrade.set_membership(&Membership {
            enrollments: vec![Enrollment {
                id: old_id.clone(),
                kind: EnrollmentKind::LegacyDesktop,
                members: vec![desktop.sign_key()],
                enrolled_at: 100,
            }],
            retired: vec![],
            policy: RingPolicy {
                inactivity_seconds: None,
                activity_key: desktop.sign_key(),
            },
            change: RosterChange::Upgrade { proofs: vec![] },
        });
        versions.push(upgrade.sign(&desktop).unwrap());
        let chain = RosterChain::from_chain(versions).unwrap();
        let profile = CompanionProfile::for_stores(
            desktop_store.to_str().unwrap().into(),
            daemon_store.to_str().unwrap().into(),
            PublicIdentity {
                name: "retained".into(),
                sign_key: desktop.sign_key(),
                noise_key: desktop.noise_key(),
            },
            PublicIdentity {
                name: "computer".into(),
                sign_key: daemon.sign_key(),
                noise_key: daemon.noise_key(),
            },
            daemon_store.join("companion.sock").to_str().unwrap().into(),
        );
        let intent = AdoptionIntent {
            operation: b64::encode(&[2; 32]),
            expires_at: 600,
            target: profile.target(),
        };
        let mut policy = CompanionPolicy::new(profile).unwrap();
        policy.start(intent.clone(), 101).unwrap();
        let mut proposal = chain.head().roster().clone();
        proposal.version += 1;
        proposal.prev = Some(chain.head().hash());
        proposal.issued_at = 102;
        proposal.members.push(Member::new(
            "computer",
            Role::Daemon,
            daemon.sign_key(),
            daemon.noise_key(),
            102,
        ));
        let mut membership = proposal.membership().unwrap();
        membership.enrollments.clear();
        membership.retired.push(old_id.clone());
        membership.enrollments.push(Enrollment {
            id: intent.operation.clone(),
            kind: EnrollmentKind::Computer,
            members: vec![desktop.sign_key(), daemon.sign_key()],
            enrolled_at: 102,
        });
        membership.change = RosterChange::CompleteComputer {
            replaces: vec![old_id],
            enrollment_id: intent.operation.clone(),
            proofs: vec![],
        };
        proposal.set_membership(&membership);
        policy
            .prepare(
                CompanionProposal {
                    operation: intent.operation.clone(),
                    predecessor: chain.versions().iter().map(|r| r.token().into()).collect(),
                    proposal,
                    transcript: ConsentTranscript::Local,
                },
                None,
                102,
            )
            .unwrap();
        let bytes = serde_json::to_vec(&policy).unwrap();
        assert!(bytes.len() > 1024 * 1024);
        eprintln!("STORAGE_COMPAT prepared companion policy bytes={} phase=Prepared no-consent-no-install", bytes.len());
        write_metadata(&daemon_store.join("companion.json"), &bytes).unwrap();
        assert!(
            reserve(&home, &desktop_store).is_ok(),
            "a valid policy's embedded predecessor is not a small reservation blob"
        );
        assert!(!daemon_store.join("keys.json").exists());
        assert_eq!(
            fs::read(daemon_store.join("companion.json")).unwrap(),
            bytes
        );
    }
    #[test]
    fn active_legacy_offer_cannot_prevent_durable_denial() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().canonicalize().unwrap();
        let desktop = home.join("desktop");
        fs::DirBuilder::new().mode(0o700).create(&desktop).unwrap();
        let dir = home.join(".xshell/daemon/ring");
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .unwrap();
        let _offer = lock(&dir.join("ring-legacy-offer.lock"), false).unwrap();
        reserve(&home, &desktop)
            .expect("an active legacy offer blocks registration, not deny-only metadata");
        assert!(dir.join(FILE).exists());
        assert!(!dir.join("companion.json").exists());
        assert!(!dir.join("keys.json").exists());
    }
    #[test]
    fn new_directory_entries_sync_each_parent_before_reporting_durable_denial() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().canonicalize().unwrap();
        let desktop = home.join("desktop");
        fs::DirBuilder::new().mode(0o700).create(&desktop).unwrap();
        let mut synced = Vec::new();
        reserve_with_sync(&home, &desktop, |path| {
            synced.push(path.to_path_buf());
            fs::File::open(path)?.sync_all()
        })
        .unwrap();
        assert_eq!(
            synced,
            [
                home.clone(),
                home.join(".xshell"),
                home.join(".xshell/daemon"),
                home.join(".xshell/daemon/ring")
            ]
        );
    }
    #[test]
    fn ancestor_sync_failure_is_propagated_and_retry_does_not_skip_existing_ancestor() {
        for failed_parent in ["", ".xshell", ".xshell/daemon", ".xshell/daemon/ring"] {
            let root = tempfile::tempdir().unwrap();
            let home = root.path().canonicalize().unwrap();
            let desktop = home.join("desktop");
            fs::DirBuilder::new().mode(0o700).create(&desktop).unwrap();
            let fail = if failed_parent.is_empty() {
                home.clone()
            } else {
                home.join(failed_parent)
            };
            let mut synced = Vec::new();
            assert!(
                reserve_with_sync(&home, &desktop, |path| {
                    synced.push(path.to_path_buf());
                    if path == fail {
                        Err(io::Error::other("injected parent fsync failure"))
                    } else {
                        fs::File::open(path)?.sync_all()
                    }
                })
                .is_err(),
                "must propagate failure syncing {failed_parent}"
            );
            assert!(synced.contains(&fail));
            let mut retried = Vec::new();
            reserve_with_sync(&home, &desktop, |path| {
                retried.push(path.to_path_buf());
                fs::File::open(path)?.sync_all()
            })
            .unwrap();
            assert!(
                retried.contains(&fail),
                "retry cannot assume a failed existing ancestor was synced"
            );
        }
    }
    #[test]
    fn oversized_metadata_is_rejected_without_unbounded_read_or_replacement() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(FILE);
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        file.set_len(1024 * 1024 + 1).unwrap(); // sparse, no megabyte payload artifact
        assert!(read_metadata(&path).is_err());
        assert_eq!(fs::metadata(&path).unwrap().len(), 1024 * 1024 + 1);
    }
    #[test]
    fn fifo_metadata_is_rejected_before_a_writer_can_unblock_open() {
        use std::os::unix::ffi::OsStrExt;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(FILE);
        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        // Rescue the pre-fix blocked open so the RED itself is bounded and joinable.
        let rescue_path = path.clone();
        let rescue = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(250));
            fs::OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(rescue_path)
                .unwrap()
        });
        let began = Instant::now();
        let result = read_metadata(&path);
        let elapsed = began.elapsed();
        let writer = rescue.join().unwrap();
        drop(writer);
        assert!(result.is_err());
        assert!(
            elapsed < Duration::from_millis(150),
            "FIFO open waited {elapsed:?} for a writer"
        );
        assert!(!fs::symlink_metadata(path).unwrap().is_file());
    }
    #[test]
    fn deny_only_is_idempotent_and_never_overwrites_malformed_or_conflicting_metadata() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().canonicalize().unwrap();
        let desktop = home.join("desktop");
        fs::DirBuilder::new().mode(0o700).create(&desktop).unwrap();
        let dir = reserve(&home, &desktop).unwrap();
        assert!(!dir.join("keys.json").exists());
        assert!(!dir.join("companion.json").exists());
        assert_eq!(reserve(&home, &desktop).unwrap(), dir);
        fs::write(dir.join(FILE), b"invalid").unwrap();
        assert!(reserve(&home, &desktop).is_err());
        assert_eq!(fs::read(dir.join(FILE)).unwrap(), b"invalid");
        fs::write(
            dir.join(FILE),
            serde_json::to_vec("/another/Desktop").unwrap(),
        )
        .unwrap();
        assert!(reserve(&home, &desktop).is_err());
    }
    #[test]
    fn symlink_or_nonprivate_root_is_refused_without_touching_target() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().canonicalize().unwrap();
        let desktop = home.join("desktop");
        fs::DirBuilder::new().mode(0o700).create(&desktop).unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), home.join(".xshell")).unwrap();
        assert!(reserve(&home, &desktop).is_err());
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
        fs::remove_file(home.join(".xshell")).unwrap();
        fs::set_permissions(&desktop, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(reserve(&home, &desktop).is_err());
    }
}
