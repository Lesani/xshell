//! Pairing over the Relay's pairing pipe (blocking): [`PairPipe`] is one socket on
//! `/v1/pair/{slot}` (section 16 of the protocol), [`pair_as_guest`] runs a joining device's
//! whole side (handshake, then [`RingClient::fetch_chain`]), and [`host_pairing`] a
//! Desktop's. The pipe is not a security boundary: the pre-shared key, the Desktop's
//! single-use and expiry bookkeeping ([`PairingHost`]) and the pinned chain are.

use super::super::chain::RosterChain;
use super::super::pairing::{
    valid_slot, Guest, Host, JoinRequest, Joined, PairError, PairRefusal, PairSecret,
    MAX_PAIR_MESSAGE,
};
use super::super::url::RelayUrl;
use super::super::{b64, DeviceKeys, NoiseKey, RingError, RingId, Role, Signer};
use super::client::{ChainPin, RingClient, RingTimeouts};
use super::transport::{self, Conn};
use super::wire::{
    decode_pair_relay, ErrorCode, PairClientFrame, PairRelayFrame, MAX_PAIR_FRAME, PING,
};
use rustls::ClientConfig;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tungstenite::{Message, WebSocket};

#[derive(Clone)]
pub struct PairOptions {
    /// TLS for `wss://`; `None`: the Mozilla roots.
    pub tls: Option<Arc<ClientConfig>>,
    /// Dialing and each handshake step once both sides are on the pipe. A guest waits this
    /// long for the Desktop's answer to its join, which the Desktop sends only once the
    /// Relay holds the new Roster version: the Desktop's wait for that
    /// (`DesktopRingConfig::pair_publish_wait`, 10 s) must stay below the guest's step.
    pub step: Duration,
    /// How often a waiting socket pings the Relay.
    pub ping_interval: Duration,
    /// The Ring client's timeouts, for fetching the chain afterwards.
    pub ring: RingTimeouts,
}

impl Default for PairOptions {
    fn default() -> Self {
        PairOptions {
            tls: None,
            step: Duration::from_secs(15),
            ping_interval: Duration::from_secs(30),
            ring: RingTimeouts::default(),
        }
    }
}

/// Polled while blocking, so a pairing can be cancelled from another thread.
pub type Cancel = Arc<AtomicBool>;

/// How often a blocked read looks at the cancel flag.
const POLL: Duration = Duration::from_millis(100);

fn relay_err(e: RingError) -> PairError {
    PairError::Relay(e.to_string())
}

/// One socket on a pairing slot.
pub struct PairPipe {
    ws: WebSocket<Conn>,
    peer: bool,
    opts: PairOptions,
    next_ping: Instant,
}

impl PairPipe {
    /// Opens `slot` on `relay_url` and reads the Relay's first word: `pair.wait` (this socket
    /// is first) or `pair.peer` (the other side is here).
    pub fn open(relay_url: &str, slot: &str, opts: &PairOptions) -> Result<PairPipe, PairError> {
        if !valid_slot(slot) {
            return Err(PairError::Invalid("bad slot".into()));
        }
        let url = RelayUrl::parse(relay_url).map_err(|e| PairError::Invalid(e.to_string()))?;
        let deadline = Instant::now() + opts.step;
        let ws = transport::dial(
            &url,
            &url.pair_endpoint(slot),
            opts.tls.clone(),
            deadline,
            MAX_PAIR_FRAME * 4,
        )
        .map_err(|e| match e {
            RingError::Timeout => PairError::Timeout,
            e => relay_err(e),
        })?;
        let mut p = PairPipe {
            ws,
            peer: false,
            opts: opts.clone(),
            next_ping: Instant::now() + opts.ping_interval,
        };
        match p.frame(deadline, None)? {
            PairRelayFrame::Wait { .. } => {}
            PairRelayFrame::Peer => p.peer = true,
            other => return Err(PairError::Relay(format!("unexpected {other:?}"))),
        }
        Ok(p)
    }

    /// Whether the other side is on the pipe.
    pub fn has_peer(&self) -> bool {
        self.peer
    }

    /// Waits for the other side until `until` ([`PairError::Expired`] after).
    pub fn wait_peer(&mut self, until: Instant, cancel: Option<&Cancel>) -> Result<(), PairError> {
        while !self.peer {
            match self.frame(until, cancel) {
                Ok(PairRelayFrame::Peer) => self.peer = true,
                Ok(other) => return Err(PairError::Relay(format!("unexpected {other:?}"))),
                Err(PairError::Timeout) => return Err(PairError::Expired),
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    pub fn send(&mut self, msg: &[u8]) -> Result<(), PairError> {
        if msg.len() > MAX_PAIR_MESSAGE {
            return Err(PairError::Payload("message too long".into()));
        }
        let text = PairClientFrame::Msg {
            payload: b64::encode(msg),
        }
        .encode();
        self.write(text)
    }

    /// The other side's next message, within one step.
    pub fn recv(&mut self, cancel: Option<&Cancel>) -> Result<Vec<u8>, PairError> {
        let until = Instant::now() + self.opts.step;
        match self.frame(until, cancel)? {
            PairRelayFrame::Msg { payload } => {
                match b64::decoded_len(payload.len()) {
                    Some(n) if n <= MAX_PAIR_MESSAGE => {}
                    _ => return Err(PairError::Payload("message too long".into())),
                }
                b64::decode(&payload).map_err(|_| PairError::Payload("encoding".into()))
            }
            other => Err(PairError::Relay(format!("unexpected {other:?}"))),
        }
    }

    pub fn close(mut self) {
        let _ = self.ws.close(None);
        let _ = self.ws.flush();
    }

    fn write(&mut self, text: String) -> Result<(), PairError> {
        let _ = self
            .ws
            .get_mut()
            .set_deadline(Instant::now() + self.opts.step);
        let mut r = self.ws.send(Message::text(text));
        loop {
            match r {
                Ok(()) => return Ok(()),
                Err(tungstenite::Error::Io(e)) if transport::is_would_block(&e) => {
                    r = self.ws.flush();
                }
                Err(e) => return Err(relay_err(transport::map_ws_error(e))),
            }
        }
    }

    /// The next frame that is not `pong` or unknown, pinging while it waits. A Relay `error`
    /// ends the pipe.
    fn frame(
        &mut self,
        until: Instant,
        cancel: Option<&Cancel>,
    ) -> Result<PairRelayFrame, PairError> {
        loop {
            if cancel.is_some_and(|c| c.load(Ordering::Acquire)) {
                return Err(PairError::Cancelled);
            }
            let now = Instant::now();
            if now >= until {
                return Err(PairError::Timeout);
            }
            if now >= self.next_ping {
                self.next_ping = now + self.opts.ping_interval;
                self.write(PING.to_string())?;
            }
            let wake = until.min(self.next_ping).min(now + POLL);
            let _ = self.ws.get_mut().set_deadline(wake);
            match self.ws.read() {
                Ok(Message::Text(t)) => match decode_pair_relay(t.as_str()) {
                    Ok(PairRelayFrame::Pong) | Ok(PairRelayFrame::Unknown { .. }) => continue,
                    Ok(PairRelayFrame::Error { code, .. }) => return Err(pipe_error(&code)),
                    Ok(f) => return Ok(f),
                    Err(e) => return Err(PairError::Relay(e.to_string())),
                },
                Ok(Message::Close(_)) => return Err(PairError::Relay("the pipe closed".into())),
                Ok(_) => continue,
                Err(tungstenite::Error::Io(e)) if transport::is_would_block(&e) => continue,
                Err(e) => return Err(relay_err(transport::map_ws_error(e))),
            }
        }
    }
}

fn pipe_error(code: &ErrorCode) -> PairError {
    match code {
        ErrorCode::PairExpired => PairError::Expired,
        // Someone else is on the slot, or it was used.
        ErrorCode::PairBusy => PairError::Refused(PairRefusal::Used),
        c => PairError::Relay(format!("relay refused: {c}")),
    }
}

/// What a joining device asks for.
pub struct GuestRequest<'a> {
    pub relay_url: &'a str,
    pub secret: &'a PairSecret,
    /// The Desktop's Noise key from a QR payload; `None` for a code.
    pub pin: Option<NoiseKey>,
    /// The Ring a QR payload names; the Desktop's answers must name it too.
    pub ring_id: Option<&'a RingId>,
    pub keys: Arc<DeviceKeys>,
    pub role: Role,
    pub name: &'a str,
    /// How long to wait for the other side when this socket is first: a QR's Desktop is
    /// already there (`ZERO` fails at once with [`PairError::NotFound`]); `xshelld pair`
    /// waits for the code to be typed.
    pub wait: Duration,
}

/// A joining device's side, from the slot to the verified chain that lists it.
pub fn pair_as_guest(
    req: &GuestRequest<'_>,
    opts: &PairOptions,
    cancel: Option<&Cancel>,
) -> Result<RosterChain, PairError> {
    let mut pipe = PairPipe::open(req.relay_url, &req.secret.slot(), opts)?;
    if !pipe.has_peer() {
        if req.wait.is_zero() {
            pipe.close();
            return Err(PairError::NotFound);
        }
        pipe.wait_peer(Instant::now() + req.wait, cancel)?;
    }
    let (mut g, m1) = Guest::start(&req.keys, req.secret, req.pin)?;
    pipe.send(&m1)?;
    let hello = g.read_hello(&pipe.recv(cancel)?)?;
    if req.ring_id.is_some_and(|r| r != &hello.ring_id) {
        return Err(PairError::Payload("another ring".into()));
    }
    let m3 = g.join(&req.keys, req.role, req.name)?;
    pipe.send(&m3)?;
    let joined = g.finish(&pipe.recv(cancel)?)?;
    pipe.close();
    if joined.ring_id != hello.ring_id {
        return Err(PairError::Payload("another ring".into()));
    }
    let signer: Arc<dyn Signer> = req.keys.clone();
    RingClient::fetch_chain(
        &joined.relay_url,
        &joined.ring_id,
        signer,
        &ChainPin {
            version: joined.version,
            hash: joined.hash.clone(),
        },
        opts.tls.clone(),
        opts.ring,
    )
    .map_err(relay_err)
}

/// The Desktop's bookkeeping, called by [`host_pairing`] once message 3 arrived.
pub trait PairingHost: Send + Sync {
    /// Uses the secret up: `Err(Used)` the second time, `Err(Expired)` after its time.
    /// Called first, before anything in the message is looked at.
    fn consume(&self) -> Result<(), PairRefusal>;
    /// Adds the device: a new Roster version, committed and published (acknowledged by the
    /// Relay) before this returns.
    fn add(&self, req: &JoinRequest) -> Result<Joined, PairRefusal>;
}

/// What the Desktop's side of a pairing is about.
pub struct HostRequest<'a> {
    pub keys: &'a DeviceKeys,
    pub secret: &'a PairSecret,
    pub ring_id: &'a RingId,
    /// This Desktop's name, in message 2.
    pub name: &'a str,
    /// The role this flow adds: `mobile` for a QR, `daemon` for a code.
    pub role: Role,
}

/// The Desktop's side over `pipe`, which already has the peer (see
/// [`PairPipe::wait_peer`]). Returns what was added, or why not.
pub fn host_pairing(
    mut pipe: PairPipe,
    req: &HostRequest<'_>,
    host: &dyn PairingHost,
    cancel: Option<&Cancel>,
) -> Result<JoinRequest, PairError> {
    if !pipe.has_peer() {
        return Err(PairError::NotFound);
    }
    let mut h = Host::start(req.keys, req.secret)?;
    h.read_start(&pipe.recv(cancel)?)?;
    let m2 = h.hello(req.ring_id, req.name)?;
    pipe.send(&m2)?;
    let m3 = pipe.recv(cancel)?;
    // The secret is spent by any third message, before it is even decrypted: a replay, or a
    // second try, finds it gone.
    let consumed = host.consume();
    let join = h.read_join(&m3)?;
    let result = consumed
        .and_then(|()| {
            if join.role == req.role {
                Ok(())
            } else {
                Err(PairRefusal::Role)
            }
        })
        .and_then(|()| host.add(&join));
    let m4 = h.answer(&result)?;
    let sent = pipe.send(&m4);
    pipe.close();
    sent?;
    match result {
        Ok(_) => Ok(join),
        Err(r) => Err(PairError::Refused(r)),
    }
}
