//! `xshelld pair [--relay URL] [--name NAME] [--force]`: adds this computer to a Ring as a
//! `daemon` member from its own side (CONTEXT.md: "other machines pair with
//! `xshelld pair`"). It shows a one-time code, waits on the Relay's pairing pipe for a
//! Desktop where the user types it, runs the pairing handshake, fetches and pins the chain
//! that lists this computer, stores it, and hands it to a running `serve` (`ring.join`).
//! Without one, it keeps the membership and says how to start the Persistent Daemon.
//!
//! Trust on disk is never rolled back: under `ring.lock` (shared with `serve`) the fetched
//! chain must extend the stored one of the same Ring, which a newer stored head wins over;
//! foreign or terminal membership requires explicit Leave; `--force` cannot replace it.
//!
//! Exit codes: 0 paired, 1 an error, 5 the code expired.

use crate::cli::Opts;
use crate::paths::Paths;
use crate::server::ring::{host_name, is_daemon_member, Store};
use serde_json::Value;
use std::io::{self, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::Duration;
use xshell_protocol::frame::{read_frame, Frame, MAX_FRAME_LEN};
use xshell_protocol::msg::{decode_server, encode_msg, ClientMsg, Hello, JoinExpect, ServerMsg};
use xshell_protocol::ring::pairing::{PairCode, PairError, PAIR_TTL_SECS};
use xshell_protocol::ring::relay::pair::{pair_as_guest_with_ready, GuestRequest, PairOptions};
use xshell_protocol::ring::url::{RelayUrl, HOSTED_RELAY_URL};
use xshell_protocol::ring::{member_name, Role, RosterChain};
use xshell_protocol::PROTOCOL;

/// The exit code when nobody entered the code in time.
pub const EXPIRED_EXIT: i32 = 5;

enum Failure {
    Expired,
    Other(String),
}

impl From<String> for Failure {
    fn from(s: String) -> Self {
        Failure::Other(s)
    }
}

pub fn run_pair(opts: &Opts, paths: &Paths) -> i32 {
    run_pair_to(opts, paths, &mut io::stdout())
}

/// [`run_pair`] writing to `out` (tests read the code from it).
pub fn run_pair_to(opts: &Opts, paths: &Paths, out: &mut dyn Write) -> i32 {
    match pair(opts, paths, out) {
        Ok(()) => 0,
        Err(Failure::Expired) => {
            say(
                out,
                "This code expired. Run xshelld pair again to get a new one.",
            );
            EXPIRED_EXIT
        }
        Err(Failure::Other(m)) => {
            eprintln!("xshelld: {m}");
            1
        }
    }
}

/// How long the code is good for: 10 minutes (`XSHELLD_PAIR_TTL_MS`, test only, shortens it).
fn ttl() -> Duration {
    std::env::var("XSHELLD_PAIR_TTL_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_secs(PAIR_TTL_SECS))
        .min(Duration::from_secs(PAIR_TTL_SECS))
}

fn say(out: &mut dyn Write, line: &str) {
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

fn pair(opts: &Opts, paths: &Paths, out: &mut dyn Write) -> Result<(), Failure> {
    let store = Store::new(paths.ring_dir.clone());
    let _offer = store.reserve_legacy_offer()?;
    if crate::server::ring::companion_registered(&paths.ring_dir) {
        return Err(Failure::Other(
            "protected_counterpart_required: registered GUI pairing must use its local controller"
                .into(),
        ));
    }
    let stored = store
        .read_chain()
        .map_err(|e| format!("the stored ring is unreadable ({e})"))?;
    if stored.is_some() {
        return Err(Failure::Other(
            "leave_required: this computer retains a Ring; --force cannot replace retained membership".into(),
        ));
    }
    let relay = opts
        .relay
        .clone()
        .unwrap_or_else(|| HOSTED_RELAY_URL.to_string());
    RelayUrl::parse(&relay).map_err(|e| e.to_string())?;
    let keys = Arc::new(store.load_or_create_keys()?);
    let name = match &opts.name {
        Some(n) => member_name(n, "this computer"),
        None => host_name(),
    };

    let code = PairCode::generate().map_err(|e| e.to_string())?;
    let secret = code.secret();
    let fetched = pair_as_guest_with_ready(
        &GuestRequest {
            relay_url: &relay,
            secret: &secret,
            pin: None,
            ring_id: None,
            keys: keys.clone(),
            role: Role::Daemon,
            name: &name,
            wait: ttl(),
        },
        &PairOptions::default(),
        None,
        || {
            say(out, &format!("Pairing code: {}", code.format()));
            say(
                out,
                "In xshell, open Settings -> Mobile -> Add a computer and enter this code.",
            );
            say(out, "It works once, for 10 minutes.");
            say(out, &format!("Relay: {relay}"));
        },
    )
    .map_err(|e| match e {
        PairError::Expired => Failure::Expired,
        e => Failure::Other(format!("pairing failed: {e}")),
    })?;
    if !is_daemon_member(&fetched, &keys) {
        return Err(Failure::Other(
            "the ring does not list this computer as a host".into(),
        ));
    }

    let chain = adopt(&store, fetched, &keys)?;
    say(out, &format!("Paired as {name}"));
    match join_running(paths, &chain, None) {
        Ok(true) => {}
        Ok(false) => {
            say(
                out,
                "xshelld isn't running. Run xshelld serve, or in xshell turn on",
            );
            say(
                out,
                "\"Keep terminals running after quit\" so your devices can reach this computer.",
            );
        }
        Err(e) => {
            say(
                out,
                &format!("Pairing was saved, but the running xshelld couldn't use it: {e}"),
            );
            say(out, "Restart xshelld to use the new pairing.");
        }
    }
    Ok(())
}

/// Stores only an initial membership or an authorized extension of the retained Ring.
/// Foreign/terminal membership is never a force-replacement or a new enrollment attempt.
fn adopt(
    store: &Store,
    fetched: RosterChain,
    keys: &xshell_protocol::ring::DeviceKeys,
) -> Result<RosterChain, Failure> {
    let _lock = store.lock()?;
    store.refuse_registered_join()?;
    let disk = store
        .read_chain()
        .map_err(|e| format!("the stored ring is unreadable ({e})"))?;
    let next = match disk {
        None => fetched,
        Some(disk) => {
            if disk.ring_id() != fetched.ring_id() || !is_daemon_member(&disk, keys) {
                return Err(Failure::Other(
                    "leave_required: retained foreign or terminal membership cannot be replaced"
                        .into(),
                ));
            }
            disk.extended(fetched.versions())
                .map_err(|e| {
                    format!(
                        "the relay's roster conflicts with the one stored here ({})",
                        e.as_code()
                    )
                })?
                .0
        }
    };
    if !is_daemon_member(&next, keys) {
        return Err(Failure::Other(
            "the retained Ring does not authorize this computer".into(),
        ));
    }
    store.write_chain(&next)?;
    Ok(next)
}

/// Sends `ring.join` with `chain` (conditional on `expect`) to the `serve` on this machine.
/// `Ok(false)`: none runs.
fn join_running(
    paths: &Paths,
    chain: &RosterChain,
    expect: Option<JoinExpect>,
) -> Result<bool, String> {
    let rosters = chain
        .versions()
        .iter()
        .map(|v| v.token().to_string())
        .collect();
    Ok(request(paths, &ClientMsg::RingJoin { rosters, expect })?.is_some())
}

/// One request to the `serve` on this machine: its answer, or `None` when none runs.
fn request(paths: &Paths, msg: &ClientMsg) -> Result<Option<Value>, String> {
    let sock = match UnixStream::connect(&paths.socket) {
        Ok(s) => s,
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(None)
        }
        Err(e) => return Err(e.to_string()),
    };
    let t = Some(Duration::from_secs(20));
    sock.set_read_timeout(t).map_err(|e| e.to_string())?;
    sock.set_write_timeout(t).map_err(|e| e.to_string())?;
    let mut w = sock.try_clone().map_err(|e| e.to_string())?;
    let mut r = BufReader::new(sock);
    let hello = ClientMsg::Hello(Hello {
        protocol: PROTOCOL,
        version: env!("CARGO_PKG_VERSION").into(),
        capabilities: Vec::new(),
        build: None,
    });
    let send = |w: &mut UnixStream, m: &ClientMsg, id: Option<u64>| -> Result<(), String> {
        let bytes = encode_msg(m, id).map_err(|e| e.to_string())?;
        w.write_all(&bytes).map_err(|e| e.to_string())
    };
    send(&mut w, &hello, None)?;
    send(&mut w, msg, Some(1))?;
    loop {
        match read_frame(&mut r, MAX_FRAME_LEN).map_err(|e| e.to_string())? {
            Some(Frame::Json(j)) => match decode_server(&j) {
                Ok(ServerMsg::Res(res)) if res.id == 1 => {
                    return res.outcome.into_result().map(Some);
                }
                Ok(ServerMsg::Error { message, .. }) => return Err(message),
                _ => continue,
            },
            Some(_) => continue,
            None => return Err("the daemon closed the connection".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xshell_protocol::ring::{DeviceKeys, Member, SignedRoster};

    fn paths(t: &tempfile::TempDir) -> Paths {
        crate::paths::resolve(t.path(), Some(&t.path().join("run")), None)
    }

    /// v1 (a Desktop) → v2 (adds `me` as a daemon) → v3 (adds a phone).
    fn chain(me: &DeviceKeys) -> (DeviceKeys, RosterChain) {
        let desk = DeviceKeys::generate().unwrap();
        let g = SignedRoster::genesis(&desk, desk.noise_key(), "d", "ws://127.0.0.1:9", 1).unwrap();
        let v2 = g
            .next(&desk, 2, |d| {
                d.add(Member::new(
                    "me",
                    Role::Daemon,
                    me.sign_key(),
                    me.noise_key(),
                    2,
                ))
            })
            .unwrap();
        let p = DeviceKeys::generate().unwrap();
        let v3 = v2
            .next(&desk, 3, |d| {
                d.add(Member::new(
                    "p",
                    Role::Mobile,
                    p.sign_key(),
                    p.noise_key(),
                    3,
                ))
            })
            .unwrap();
        (desk, RosterChain::from_chain(vec![g, v2, v3]).unwrap())
    }

    fn prefix(c: &RosterChain, n: usize) -> RosterChain {
        RosterChain::from_chain(c.versions()[..n].to_vec()).unwrap()
    }

    #[test]
    fn an_older_pinned_head_never_overwrites_newer_local_trust() {
        let t = tempfile::tempdir().unwrap();
        let p = paths(&t);
        let store = Store::new(p.ring_dir.clone());
        let me = store.load_or_create_keys().unwrap();
        let (_, full) = chain(&me);
        store.write_chain(&full).unwrap();
        // The Relay showed only up to v2 (it hid v3): the stored v3 stays.
        let kept = adopt(&store, prefix(&full, 2), &me).ok().unwrap();
        assert_eq!(kept, full);
        assert_eq!(store.read_chain().unwrap().unwrap(), full);
        // A newer one extends what is stored.
        store.write_chain(&prefix(&full, 2)).unwrap();
        assert_eq!(adopt(&store, full.clone(), &me).ok().unwrap(), full);
        assert_eq!(store.read_chain().unwrap().unwrap(), full);
    }

    #[test]
    fn terminal_membership_is_not_automatically_reenrolled() {
        // Historical v1 file fixture: the local key has already been removed.
        let t = tempfile::tempdir().unwrap();
        let p = paths(&t);
        let store = Store::new(p.ring_dir.clone());
        let me = store.load_or_create_keys().unwrap();
        let (desktop, full) = chain(&me);
        let removed = full
            .head()
            .next(&desktop, 4, |draft| {
                draft.remove(&me.sign_key());
            })
            .unwrap();
        let mut terminal = full.clone();
        terminal.accept(&[removed]).unwrap();
        store.write_chain(&terminal).unwrap();
        assert!(
            matches!(adopt(&store, full, &me), Err(Failure::Other(message)) if message.starts_with("leave_required:"))
        );
        assert_eq!(store.read_chain().unwrap().unwrap(), terminal);
        assert_eq!(
            store.load_keys().unwrap().unwrap().sign_key(),
            me.sign_key()
        );
    }

    #[test]
    fn forks_and_other_rings_are_refused() {
        let t = tempfile::tempdir().unwrap();
        let p = paths(&t);
        let store = Store::new(p.ring_dir.clone());
        let me = store.load_or_create_keys().unwrap();
        let (desk, full) = chain(&me);
        store.write_chain(&full).unwrap();
        // Another v3 of the same Ring: a fork.
        let other = DeviceKeys::generate().unwrap();
        let fork_v3 = full
            .get(2)
            .unwrap()
            .next(&desk, 9, |d| {
                d.add(Member::new(
                    "o",
                    Role::Mobile,
                    other.sign_key(),
                    other.noise_key(),
                    9,
                ))
            })
            .unwrap();
        let fork = RosterChain::from_chain(vec![
            full.get(1).unwrap().clone(),
            full.get(2).unwrap().clone(),
            fork_v3,
        ])
        .unwrap();
        assert!(
            matches!(adopt(&store, fork, &me), Err(Failure::Other(m)) if m.contains("conflicts"))
        );
        assert_eq!(store.read_chain().unwrap().unwrap(), full);
        // `--force` is not a Leave and cannot bypass retained foreign membership.
        let (_, theirs) = chain(&me);
        assert!(
            matches!(adopt(&store, theirs.clone(), &me), Err(Failure::Other(m)) if m.starts_with("leave_required:"))
        );
        assert_eq!(store.read_chain().unwrap().unwrap(), full);
        assert!(
            matches!(adopt(&store, theirs, &me), Err(Failure::Other(m)) if m.starts_with("leave_required:"))
        );
        assert_eq!(store.read_chain().unwrap().unwrap(), full);
    }
    #[test]
    fn force_pair_refuses_before_offer_and_preserves_retained_keys() {
        let t = tempfile::tempdir().unwrap();
        let paths = paths(&t);
        let store = Store::new(paths.ring_dir.clone());
        let keys = store.load_or_create_keys().unwrap();
        let (_, retained) = chain(&keys);
        store.write_chain(&retained).unwrap();
        for force in [false, true] {
            let mut output = Vec::new();
            let options = Opts {
                force,
                ..Default::default()
            };
            assert!(
                matches!(pair(&options, &paths, &mut output), Err(Failure::Other(message)) if message.starts_with("leave_required:"))
            );
            assert!(
                output.is_empty(),
                "no code or network offer before the membership fence"
            );
            assert_eq!(store.read_chain().unwrap().unwrap(), retained);
            assert_eq!(
                store.load_keys().unwrap().unwrap().sign_key(),
                keys.sign_key()
            );
        }
    }
    #[test]
    fn stopped_or_unreadable_registration_never_falls_back_to_headless_offer() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(&temp);
        let store = Store::new(paths.ring_dir.clone());
        store.ensure_dir().unwrap();
        std::fs::write(paths.ring_dir.join("companion.json"), b"{unreadable").unwrap();
        for force in [false, true] {
            let mut output = Vec::new();
            let options = Opts {
                force,
                ..Default::default()
            };
            assert!(
                matches!(pair(&options, &paths, &mut output), Err(Failure::Other(message)) if message.starts_with("protected_counterpart_required:"))
            );
            assert!(output.is_empty());
            assert!(
                !paths.ring_dir.join("keys.json").exists(),
                "refusal must not create replacement identities"
            );
        }
    }
    #[test]
    fn legacy_adopt_rechecks_effective_binding_under_ring_lock() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(&temp);
        let store = Store::new(paths.ring_dir.clone());
        let keys = store.load_or_create_keys().unwrap();
        let (_, fetched) = chain(&keys); // Historical v1 legacy negative, not v2 bootstrap.
        {
            let _lock = store.lock().unwrap();
            crate::server::ring::write_private(
                &paths.ring_dir.join("companion.json"),
                b"retained binding",
            )
            .unwrap();
        }
        assert!(
            matches!(adopt(&store, fetched, &keys), Err(Failure::Other(message)) if message.starts_with("protected_counterpart_required:"))
        );
        assert!(store.read_chain().unwrap().is_none());
        assert_eq!(
            store.load_keys().unwrap().unwrap().sign_key(),
            keys.sign_key()
        );
    }
}
