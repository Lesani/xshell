//! A single owner HostLink over the elected Connector's authenticated Sessions hub.
use super::hosts::{OwnerRouting, Reachability, VerifiedRingHost};
use crate::cancel::{CancelHook, CancelToken};
use crate::dial::{Connection, Diagnosis, DialError, Dialed, Dialer};
use crate::link::LinkIo;
use std::sync::Arc;
use xshell_protocol::ring::relay::sessions::SessionStream;

#[derive(Clone)]
pub(crate) struct RelayDialer {
    pub host: VerifiedRingHost,
    pub owner: Arc<OwnerRouting>,
}
impl RelayDialer {
    pub fn authorized(&self) -> bool {
        self.owner.authorized(&self.host)
    }
    pub fn same_route(&self, other: &Self) -> bool {
        self.owner.generation == other.owner.generation
            && Arc::ptr_eq(&self.owner, &other.owner)
            && self.host.sign_key == other.host.sign_key
            && self.host.noise_key == other.host.noise_key
            && self.host.enrollment == other.host.enrollment
    }
}
impl Dialer for RelayDialer {
    fn dial(&self, cancel: &CancelToken) -> Result<Dialed, DialError> {
        let cancelled = || cancel.is_cancelled() || !self.authorized();
        if cancelled() {
            return Err(DialError::Cancelled);
        }
        // Signed members stay visible offline, but never start a fresh Noise Open there.
        if self.owner.reachability(&self.host) != Reachability::Online {
            return Err(DialError::failed("the Ring Host is not online", None));
        }
        let open_cancelled =
            || cancelled() || self.owner.reachability(&self.host) != Reachability::Online;
        let stream = self
            .owner
            .sessions
            .open_cancellable(&self.host.sign_key, &open_cancelled)
            .map_err(|e| {
                if cancelled() {
                    DialError::Cancelled
                } else {
                    DialError::failed(e.to_string(), None)
                }
            })?;
        if open_cancelled() {
            stream.abort("Relay dial cancelled");
            return Err(if cancelled() {
                DialError::Cancelled
            } else {
                DialError::failed("the Ring Host went offline", None)
            });
        }
        let close = stream.try_clone();
        let hook = cancel.on_cancel(move || close.abort("Relay dial cancelled"));
        let close = stream.try_clone();
        let owner_hook = self
            .owner
            .stopped
            .on_cancel(move || close.abort("Relay dial cancelled"));
        Ok(Dialed {
            io: LinkIo {
                read: Box::new(stream.try_clone()),
                write: Box::new(stream.try_clone()),
            },
            conn: Box::new(RelayConnection {
                stream,
                _hook: hook,
                _owner_hook: owner_hook,
            }),
        })
    }
    fn describe(&self) -> String {
        format!("Ring Host {}", self.host.id())
    }
    fn authorized(&self) -> bool {
        self.authorized()
    }
}
struct RelayConnection {
    stream: SessionStream,
    _hook: CancelHook,
    _owner_hook: CancelHook,
}
impl Connection for RelayConnection {
    fn pid(&self) -> Option<u32> {
        None
    }
    fn end(&self) {
        self.stream.abort("Relay dial cancelled");
    }
    fn diagnose(&self, message: String) -> Diagnosis {
        Diagnosis {
            message,
            hint: None,
            reinstall: false,
        }
    }
}
impl Drop for RelayConnection {
    fn drop(&mut self) {
        self.end();
    }
}
