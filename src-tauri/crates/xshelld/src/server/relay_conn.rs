//! Sessions through the Relay as Daemon connections. Each Connector gets its own
//! [`Sessions`] (built by [`Hub::sessions`]); a session a Desktop or Mobile opens becomes one
//! more protocol connection, served by [`conn::handle`] with the role its Roster entry
//! maps to (`desktop` → [`Role::Desktop`], `mobile` → [`Role::Mobile`]; a `daemon` peer is
//! refused in the handshake). `conn` is unchanged: a socket pair bridges it to the session.
//!
//! Two bridge threads per session: one reads the session and writes the pair (so the Relay's
//! IO thread never blocks on a slow connection; the session's bounded queue sits in
//! between), one reads the pair and writes the session (waiting out a full Relay queue up to
//! the write stall timeout, so the connection's Outbox caps apply). Whichever side ends
//! first ends the other: a killed session shuts the pair down (a third thread waits for
//! that, so even a write blocked on a connection that stopped reading is interrupted) and
//! `conn` cleans up and detaches; a session closed in order is drained into the pair
//! first; a connection that ends (its hello timeout included) closes the session.

use super::conn;
use super::registry::Daemon;
use super::role::Role;
use super::transport;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;
use xshell_protocol::ring::noise::MAX_STREAM_CHUNK;
use xshell_protocol::ring::relay::sessions::{Incoming, Sessions, SessionsConfig};
use xshell_protocol::ring::{DeviceKeys, Role as RingRole};

pub(crate) struct Hub {
    daemon: OnceLock<Weak<Daemon>>,
    write_stall: Duration,
}

impl Hub {
    pub fn new(write_stall: Duration) -> Hub {
        Hub {
            daemon: OnceLock::new(),
            write_stall,
        }
    }

    /// The Daemon accepted sessions are served by; set once it exists.
    pub fn bind(&self, d: Weak<Daemon>) {
        let _ = self.daemon.set(d);
    }

    /// Sessions for one Connector, answering with this Host's keys.
    pub fn sessions(&self, keys: Arc<DeviceKeys>) -> Sessions {
        let mut cfg = SessionsConfig::new(keys);
        cfg.write_stall = self.write_stall;
        let daemon = self.daemon.get().cloned().unwrap_or_default();
        Sessions::new(
            cfg,
            Some(Arc::new(move |inc: Incoming| {
                let Some(d) = daemon.upgrade() else {
                    inc.stream.close("xshelld is exiting");
                    return;
                };
                if let Err(e) = bridge(d, inc) {
                    crate::log!("WARN", "ring: cannot serve a relay session: {e}");
                }
            })),
        )
    }
}

fn bridge(d: Arc<Daemon>, inc: Incoming) -> io::Result<()> {
    let role = match inc.role {
        RingRole::Desktop => Role::Desktop,
        RingRole::Mobile => Role::Mobile,
        // `session_role` never accepts one; refuse rather than guess.
        RingRole::Daemon => {
            inc.stream.close("forbidden");
            return Ok(());
        }
    };
    if d.exiting.load(Ordering::SeqCst) {
        inc.stream.close("xshelld is exiting");
        return Ok(());
    }
    // One end for `conn`, one for the bridge.
    let (ours, theirs) = match transport::pair() {
        Ok(p) => p,
        Err(e) => {
            inc.stream.close("cannot serve");
            return Err(e);
        }
    };
    let id = d.next_conn.fetch_add(1, Ordering::SeqCst);
    let peer = inc.member.clone();
    crate::log!(
        "INFO",
        "conn {id}: relay session from {} ({:?})",
        inc.member.name,
        role
    );
    let mut from_session = inc.stream.try_clone();
    let to_session = inc.stream;
    let (w, r, cut) = (ours.try_clone()?, ours.try_clone()?, ours);
    let watched = to_session.try_clone();
    let started = std::thread::Builder::new()
        .name(format!("conn-{id}-r"))
        .spawn({
            let d = d.clone();
            move || conn::handle(d, theirs, id, role, Some(peer))
        })
        .and_then(|_| {
            std::thread::Builder::new()
                .name(format!("relay-{id}-in"))
                .spawn(move || {
                    let mut w = w;
                    let mut buf = vec![0u8; MAX_STREAM_CHUNK];
                    loop {
                        match from_session.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                if w.write_all(&buf[..n]).is_err() {
                                    from_session.close("disconnected");
                                    break;
                                }
                            }
                        }
                    }
                    // End of stream for `conn` after the last bytes; its answers still drain
                    // (a pipe on Windows has no half-close: there it ends both ways).
                    let _ = w.shutdown(Shutdown::Write);
                })
        });
    let started = started.and_then(|_| {
        let s = to_session.try_clone();
        std::thread::Builder::new()
            .name(format!("relay-{id}-out"))
            .spawn(move || {
                let mut r = r;
                let mut s = s;
                let mut buf = vec![0u8; MAX_STREAM_CHUNK];
                // Once the session is gone, what `conn` still answers is dropped, but read
                // until it ends: closing the pair here could cut off requests it has yet to
                // read from a session that was closed in order.
                let mut open = true;
                loop {
                    match r.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if open && s.write_all(&buf[..n]).is_err() {
                                open = false;
                            }
                        }
                    }
                }
                s.close("disconnected");
                let _ = r.shutdown(Shutdown::Both);
            })
    });
    let started = started.and_then(|_| {
        std::thread::Builder::new()
            .name(format!("relay-{id}-cut"))
            .spawn(move || {
                // A killed session ends the pair at once: a write blocked on a connection
                // that stopped reading fails, and `conn` sees the end. An orderly close
                // drains first: the inbound thread delivers the last bytes, then ends it.
                while !watched.wait_closed(Duration::from_secs(3600)) {}
                if watched.is_killed() {
                    let _ = cut.shutdown(Shutdown::Both);
                }
            })
    });
    if let Err(e) = started {
        to_session.close("cannot serve");
        return Err(e);
    }
    Ok(())
}
