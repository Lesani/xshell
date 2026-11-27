//! Genuine historical v1 code/Noise/Store/Host-join controls. Compiled only with the
//! explicit non-default legacy-pair-test-control feature; never a CLI alternative.
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
use xshell_protocol::ring::pairing::{PairCode, PairError};
use xshell_protocol::ring::relay::pair::{pair_as_guest_with_ready, GuestRequest, PairOptions};
use xshell_protocol::ring::url::{RelayUrl, HOSTED_RELAY_URL};
use xshell_protocol::ring::{member_name, Role, RosterChain};
use xshell_protocol::PROTOCOL;

enum Failure {
    Expired,
    Other(String),
}
impl From<String> for Failure {
    fn from(s: String) -> Self {
        Self::Other(s)
    }
}
/// Historical v1 test control only. No production CLI dispatch reaches this helper.
pub fn run_legacy_pair_test_control(
    opts: &Opts,
    paths: &Paths,
    out: &mut dyn Write,
    lifetime: Duration,
    cancel: &xshell_protocol::ring::relay::pair::Cancel,
) -> Result<(), String> {
    pair(opts, paths, out, lifetime, cancel).map_err(|failure| match failure {
        Failure::Expired => "expired".into(),
        Failure::Other(message) => message,
    })
}

fn say(out: &mut dyn Write, line: &str) {
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

fn pair(
    opts: &Opts,
    paths: &Paths,
    out: &mut dyn Write,
    lifetime: Duration,
    cancel: &xshell_protocol::ring::relay::pair::Cancel,
) -> Result<(), Failure> {
    let store = Store::new(paths.ring_dir.clone());
    xshell_core::companion_reservation::validate_daemon_store(&paths.ring_dir)
        .map_err(|e| format!("protected_counterpart_required: unsafe local Store: {e}"))?;
    if crate::server::ring::companion_registered(&paths.ring_dir) {
        return Err(Failure::Other(
            "protected_counterpart_required: legacy test control refuses registered Computer"
                .into(),
        ));
    }
    let _offer = store.reserve_legacy_offer()?;
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
            wait: lifetime,
        },
        &PairOptions::default(),
        Some(cancel),
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
    .map_err(|e| {
        if legacy_outcome_uncertain(&store) {
            return uncertain_legacy_failure(format!("pairing ended: {e}"));
        }
        match e {
            PairError::Expired => Failure::Expired,
            e => Failure::Other(format!("pairing failed: {e}")),
        }
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

fn legacy_outcome_uncertain(store: &Store) -> bool {
    xshell_core::companion_reservation::read_reservation(&store.dir)
        .ok()
        .flatten()
        .is_some_and(|reservation| reservation.legacy_outcome_uncertain)
}
fn uncertain_legacy_failure(message: String) -> Failure {
    Failure::Other(format!("reconciliation_required: legacy pairing remote outcome is uncertain; startup denial does not revoke a transmitted proof or establish nonmembership ({message})"))
}

/// Stores only an initial membership or an authorized extension of the retained Ring.
/// Foreign/terminal membership is never a force-replacement or a new enrollment attempt.
fn adopt(
    store: &Store,
    fetched: RosterChain,
    keys: &xshell_protocol::ring::DeviceKeys,
) -> Result<RosterChain, Failure> {
    let _lock = store.lock()?;
    store.refuse_registered_join().map_err(|error| {
        if legacy_outcome_uncertain(store) {
            uncertain_legacy_failure(error)
        } else {
            Failure::Other(error)
        }
    })?;
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
