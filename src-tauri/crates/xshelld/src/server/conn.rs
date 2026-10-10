//! One connection: handshake, then a read loop that applies each message its [`Role`] allows.
//! Every handler is non-blocking (PTY writes go through input threads, calls get their own
//! thread). Losing a connection detaches it everywhere and ends nothing.

use super::agent;
use super::calls::spawn_call;
use super::outbox::{writer_loop, Outbox, PaceCfg};
use super::prompt_cell::InputCarry;
use super::registry::{frame, now_ms, Daemon};
use super::relaunch;
use super::role::{self, Role};
use super::terminal::{self, SessionHold, Terminal};
use super::transport::Stream;
use super::{ConnId, ExitReason, TestPoint};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::BufReader;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;
use xshell_protocol::frame::{read_frame, Frame, MAX_FRAME_LEN};
use xshell_protocol::msg::{
    decode_inbound, encode_res, submit_reply, ClientMsg, DecodeError, Hello, Inbound, OpenReply,
    OpenSpec, ServerMsg, SESSION_CLOSING, SESSION_OPEN,
};
use xshell_protocol::negotiate::negotiate;
use xshell_protocol::ring::Member;
use xshell_protocol::{CAPABILITIES, OPEN_INDETERMINATE};

/// The answer to `daemon.upgrade` on a GUI-bound Daemon. A remote Desktop shows it.
pub(crate) const UPGRADE_REFUSED: &str =
    "xshell on this machine runs these terminals; update xshell there instead";

pub(crate) fn reply(ob: &Outbox, id: Option<u64>, r: Result<Value, String>) {
    if let Some(id) = id {
        ob.push_control(Arc::from(encode_res(id, r)));
    }
}

fn push_error(ob: &Outbox, code: &str, message: String) {
    if let Some(f) = frame(&ServerMsg::Error {
        code: code.into(),
        message,
    }) {
        ob.push_control(f);
    }
}

struct Conn {
    d: Arc<Daemon>,
    id: ConnId,
    /// Set by the transport; no message changes it.
    role: Role,
    /// The Ring member a Relay session came from, as the head listed it; `None` for the
    /// local socket and SSH.
    peer: Option<Member>,
    ob: Arc<Outbox>,
    /// Terminals this connection attached to or sized; all are released on disconnect.
    touched: HashSet<Uuid>,
    inflight: Arc<AtomicUsize>,
    /// Per Terminal: whether this connection's last input ended inside an escape sequence.
    carry: HashMap<Uuid, InputCarry>,
}

pub(crate) fn handle(d: Arc<Daemon>, sock: Stream, id: ConnId, role: Role, peer: Option<Member>) {
    let (wsock, asock) = match (sock.try_clone(), sock.try_clone()) {
        (Ok(w), Ok(a)) => (w, a),
        _ => return,
    };
    // A Mobile's output is paced: it travels the Relay, envelope by envelope.
    let pace = (role == Role::Mobile).then_some(PaceCfg {
        idle: d.cfg.mobile_frame_idle,
        burst: d.cfg.mobile_frame_burst,
        window: d.cfg.mobile_burst_window,
    });
    let ob = Outbox::with_pace(
        d.cfg.conn_output_cap,
        d.cfg.conn_total_cap,
        Some(asock),
        pace,
    );
    let (obw, stall) = (ob.clone(), d.cfg.write_stall_timeout);
    if std::thread::Builder::new()
        .name(format!("conn-{id}-w"))
        .spawn(move || writer_loop(obw, wsock, stall))
        .is_err()
    {
        return;
    }
    if let Some(f) = frame(&ServerMsg::Hello(Hello {
        protocol: d.cfg.protocol,
        version: env!("CARGO_PKG_VERSION").into(),
        capabilities: CAPABILITIES
            .iter()
            .filter(|c| !d.cfg.hide_capabilities.iter().any(|h| h == *c))
            .map(|s| s.to_string())
            .collect(),
    })) {
        ob.push_control(f);
    }

    let mut reader = BufReader::new(&sock);
    let _ = sock.set_read_timeout(Some(d.cfg.hello_timeout));
    let theirs = match read_frame(&mut reader, MAX_FRAME_LEN) {
        Ok(Some(Frame::Json(j))) => match decode_inbound(&j) {
            Ok(Inbound {
                msg: ClientMsg::Hello(h),
                ..
            }) => Some(h),
            _ => None,
        },
        _ => None,
    };
    let Some(theirs) = theirs else {
        push_error(
            &ob,
            "expected_hello",
            "the first message must be hello".into(),
        );
        ob.close();
        return;
    };
    if let Err(m) = negotiate(d.cfg.protocol, theirs.protocol) {
        push_error(&ob, "protocol_mismatch", m.to_string());
        ob.close();
        return;
    }
    let _ = sock.set_read_timeout(None);
    {
        let mut reg = d.reg.lock().unwrap();
        if reg.closed {
            drop(reg);
            push_error(&ob, "shutting_down", "xshelld is exiting".into());
            ob.close();
            return;
        }
        reg.conns.insert(
            id,
            super::registry::Peer {
                ob: ob.clone(),
                role,
            },
        );
        d.touch_idle(&mut reg);
        if let Some(f) = d.terminals_frame(&reg, role) {
            ob.push_terminals(f);
        }
    }

    let mut c = Conn {
        d: d.clone(),
        id,
        role,
        peer,
        ob: ob.clone(),
        touched: HashSet::new(),
        inflight: Arc::new(AtomicUsize::new(0)),
        carry: HashMap::new(),
    };
    // A clean EOF lets queued replies drain; a protocol error drops them.
    let mut clean = false;
    loop {
        match read_frame(&mut reader, MAX_FRAME_LEN) {
            Ok(Some(Frame::Json(j))) => match decode_inbound(&j) {
                Ok(m) => c.on_msg(m),
                Err(DecodeError::Malformed(e)) => {
                    crate::log!("INFO", "conn {id}: malformed message ({e}); closing");
                    break;
                }
                Err(
                    e @ (DecodeError::UnknownType { id, .. } | DecodeError::Invalid { id, .. }),
                ) => reply(&ob, id, Err(e.to_string())),
            },
            Ok(Some(Frame::Output { .. })) => {
                crate::log!("INFO", "conn {id}: output frame from a Desktop; closing");
                break;
            }
            Ok(Some(Frame::Unknown { kind, .. })) => {
                crate::log!("INFO", "conn {id}: skipping frame of unknown kind {kind}");
            }
            Ok(None) => {
                clean = true;
                break;
            }
            Err(e) => {
                crate::log!("INFO", "conn {id}: {e}; closing");
                break;
            }
        }
        if !ob.is_open() {
            break;
        }
    }

    // Cleanup: detach everywhere, deregister. Terminals are never touched. Under the registry
    // lock, like every attach and detach, so a Relaunch never hands this connection over.
    {
        let mut reg = d.reg.lock().unwrap();
        reg.conns.remove(&id);
        d.touch_idle(&mut reg);
        for t in c.touched.iter().filter_map(|t| reg.terminals.get(t)) {
            // The size went back to a Desktop: persist it like a resize.
            if t.detach(id) {
                t.schedule_persist(&d);
            }
        }
    }
    // After the connection left the registry: no stream result is queued for it from now on.
    d.session_streams.drop_conn(id);
    if clean {
        ob.close();
    } else {
        ob.abort();
    }
}

/// Refuse a `term.open` whose Terminal started but is not kept: it is ended, and the
/// refusal is sent once its processes are gone. If that cannot be confirmed in time (or not
/// waited for at all), the error says the outcome is open ([`OPEN_INDETERMINATE`]).
fn refuse_after_end(
    d: &Arc<Daemon>,
    ob: &Arc<Outbox>,
    id: Option<u64>,
    t: Arc<Terminal>,
    e: String,
) {
    let wait = d
        .cfg
        .refused_open_wait
        .unwrap_or(d.cfg.kill_grace + Duration::from_secs(1));
    let (grace, ob2, d2, t2) = (d.cfg.kill_grace, ob.clone(), d.clone(), t.clone());
    let r = std::thread::Builder::new()
        .name("refused-open".into())
        .spawn(move || {
            d2.test_point(t2.id, TestPoint::RefusedOpen);
            let gone = t2.end_and_wait(grace, Instant::now() + wait);
            let msg = if gone {
                e
            } else {
                format!("{OPEN_INDETERMINATE} {e}")
            };
            reply(&ob2, id, Err(msg));
        });
    if r.is_err() {
        t.kill(d.cfg.kill_grace);
        reply(
            ob,
            id,
            Err(format!(
                "{OPEN_INDETERMINATE} cannot wait for the terminal to end"
            )),
        );
    }
}

impl Conn {
    /// Run `f` on the Terminal listed under `id`, if this connection's role may act on it,
    /// with the registry still locked. Attach, detach and resize go through here: a Relaunch
    /// moves the attached connections and the size arbiter to the replacement under the same
    /// lock, so none of them lands on the Terminal it replaced.
    fn with_listed<R>(&self, id: &Uuid, f: impl FnOnce(&Arc<Terminal>) -> R) -> Result<R, String> {
        let reg = self.d.reg.lock().unwrap();
        role::listed(&reg, self.role, id).map(f)
    }

    /// The Terminal listed under `id`, if this connection's role may act on it. The caller
    /// keeps using this instance rather than looking the UUID up again.
    fn terminal(&self, id: &Uuid) -> Result<Arc<Terminal>, String> {
        self.with_listed(id, Arc::clone)
    }

    /// The answer to a `term.open` whose agent session `owner` already holds (registry
    /// locked): `owner` itself when the client asked for it and may see it, else a refusal.
    /// Nothing is started, saved or broadcast.
    fn open_existing(
        &self,
        spec: &OpenSpec,
        owner: &Arc<Terminal>,
        hold: SessionHold,
    ) -> Result<Value, String> {
        if hold == SessionHold::Closing {
            return Err(SESSION_CLOSING.into());
        }
        let visible = role::sees(self.role, &owner.spec());
        if spec.adopt_existing && visible && spec.first_message.is_none() {
            let r = OpenReply {
                pid: owner.info().pid,
                terminal: Some(owner.id),
                existed: true,
            };
            return Ok(json!(r));
        }
        // A client never learns the UUID of a Terminal it may not see.
        Err(if visible {
            format!("{SESSION_OPEN}: {}", owner.id)
        } else {
            SESSION_OPEN.into()
        })
    }

    fn on_msg(&mut self, m: Inbound) {
        let id = m.id;
        let d = self.d.clone();
        // Before any lock or in-flight slot is taken; a refusal without `id` is dropped.
        if let Err(e) = role::check(self.role, &d.ctx, &m.msg) {
            reply(&self.ob, id, Err(e));
            return;
        }
        match m.msg {
            ClientMsg::Hello(_) => reply(&self.ob, id, Err("already said hello".into())),
            ClientMsg::Call { method, params } => {
                spawn_call(&d, &self.ob, &self.inflight, self.role, id, method, params)
            }
            ClientMsg::TermOpen { spec } => {
                // A first message is checked for every role, after the role's own checks and
                // before the registry is locked: a refusal here starts nothing.
                if let Some(msg) = spec.first_message.as_deref() {
                    if let Err(e) = xshell_core::first_message_args(&d.ctx, &spec.launch, msg) {
                        reply(&self.ob, id, Err(e));
                        return;
                    }
                }
                // The agent session this open resumes. Looking for its holder and starting the
                // Terminal are one step under one registry lock: two opens of one session at
                // once start one agent (capability `term.open-existing`).
                let session = spec
                    .launch
                    .agent_session()
                    .map(|(a, s)| (a.to_string(), s.to_string()));
                d.test_point(spec.terminal, TestPoint::OpenDecide);
                let mut reg = d.reg.lock().unwrap();
                if !reg.frozen && !reg.terminals.contains_key(&spec.terminal) {
                    if let Some((agent, sid)) = &session {
                        if let Some((owner, hold)) = d.session_owner(&reg, agent, sid) {
                            let r = self.open_existing(&spec, &owner, hold);
                            drop(reg);
                            reply(&self.ob, id, r);
                            return;
                        }
                        d.test_point(spec.terminal, TestPoint::OpenLooked);
                    }
                }
                let r = if reg.frozen {
                    Err(("xshelld is upgrading or shutting down".to_string(), None))
                } else if reg.terminals.contains_key(&spec.terminal) {
                    Err((format!("terminal {} already exists", spec.terminal), None))
                } else if let Err(e) = d.check_budget(&reg, spec.terminal, &spec.launch, &spec.meta)
                {
                    Err((e, None))
                } else {
                    terminal::spawn_with(
                        &d,
                        spec.terminal,
                        spec.launch,
                        spec.meta,
                        (spec.cols, spec.rows),
                        now_ms(),
                        spec.first_message.as_deref(),
                        |_| {},
                    )
                    .map_err(|e| (e.message, e.started))
                };
                match r {
                    Ok(t) => {
                        let pid = t.info().pid;
                        reg.terminals.insert(t.id, t.clone());
                        // The open succeeds only once the Terminal is in the state file, so
                        // a client that got OK can rely on it surviving a Daemon restart.
                        // Otherwise it is ended unlisted (its exit is then not published).
                        if let Err(e) = d.try_persist(&reg) {
                            reg.terminals.remove(&t.id);
                            drop(reg);
                            refuse_after_end(&d, &self.ob, id, t, e);
                            return;
                        }
                        d.touch_idle(&mut reg);
                        d.broadcast_terminals(&reg);
                        drop(reg);
                        d.last_lines.request(t.id);
                        let r = OpenReply {
                            pid,
                            terminal: Some(t.id),
                            existed: false,
                        };
                        reply(&self.ob, id, Ok(json!(r)));
                    }
                    // The process started but its threads did not: it is being ended.
                    Err((e, Some(t))) => {
                        drop(reg);
                        refuse_after_end(&d, &self.ob, id, t, e);
                    }
                    Err((e, None)) => {
                        drop(reg);
                        reply(&self.ob, id, Err(e));
                    }
                }
            }
            ClientMsg::TermAttach { terminal } => {
                let r =
                    self.with_listed(&terminal, |t| (t.clone(), t.attach(self.id, &self.ob, id)));
                match r {
                    Ok((t, (dropped, alone))) => {
                        self.touched.insert(terminal);
                        d.nudge_overflowed(dropped);
                        // A Mobile's attach would reflow every other attached screen; it
                        // relies on the replay unless nobody else is watching.
                        if self.role == Role::Desktop || alone {
                            t.nudge(d.cfg.nudge_delay);
                        }
                    }
                    Err(e) => reply(&self.ob, id, Err(e)),
                }
            }
            ClientMsg::TermDetach { terminal } => {
                let r = self.with_listed(&terminal, |t| {
                    if t.detach(self.id) {
                        t.schedule_persist(&d);
                    }
                    Value::Null
                });
                reply(&self.ob, id, r);
            }
            ClientMsg::TermInput { terminal, data } => {
                self.touched.insert(terminal);
                // Not under the registry lock: input is the hot path. Input that reaches a
                // Terminal a Relaunch is replacing is dropped like input to an ended one.
                let may_answer = self
                    .carry
                    .entry(terminal)
                    .or_default()
                    .may_answer(data.as_bytes());
                let r = self.terminal(&terminal).and_then(|t| {
                    // A Mobile's echo comes back faster for a while.
                    self.ob.note_input();
                    // Queued (and the prompt it may answer spent) before the Agent Status
                    // follows it: a status change unlists the prompt, which must already be
                    // recorded as typed at for the recheck.
                    let bytes = data.as_bytes().to_vec();
                    let mobile = self.role == Role::Mobile;
                    let written = t.write_input(&d, self.id, data, mobile, may_answer);
                    t.note_input(&d, &bytes);
                    // Typing can hand the size to this connection; persist it like a resize.
                    if written? {
                        t.schedule_persist(&d);
                    }
                    Ok(Value::Null)
                });
                reply(&self.ob, id, r);
            }
            ClientMsg::TermResize {
                terminal,
                cols,
                rows,
            } => {
                self.touched.insert(terminal);
                let r = self
                    .with_listed(&terminal, |t| {
                        t.resize(self.id, cols, rows, self.role == Role::Mobile)
                            .map(|changed| (t.clone(), changed))
                    })
                    .and_then(|r| r)
                    .map(|(t, changed)| {
                        if changed {
                            t.schedule_persist(&d);
                        }
                        Value::Null
                    });
                reply(&self.ob, id, r);
            }
            ClientMsg::TermClose { terminal } => {
                let mut reg = d.reg.lock().unwrap();
                let r = match role::listed(&reg, self.role, &terminal).cloned() {
                    Err(e) => Err(e),
                    Ok(t) if t.is_exited() => {
                        // Nothing left to signal; the pid may already be reused.
                        if !reg.frozen {
                            reg.terminals.remove(&terminal);
                            d.persist(&reg);
                            d.touch_idle(&mut reg);
                            d.broadcast_terminals(&reg);
                        }
                        Ok(Value::Null)
                    }
                    Ok(t) => {
                        t.kill(d.cfg.kill_grace);
                        Ok(Value::Null)
                    }
                };
                drop(reg);
                reply(&self.ob, id, r);
            }
            ClientMsg::TermUpdate {
                terminal,
                session_id,
                meta,
            } => {
                let reg = d.reg.lock().unwrap();
                let r = match reg.terminals.get(&terminal) {
                    None => Err(format!("unknown terminal {terminal}")),
                    Some(t) => match (session_id.as_deref(), t.agent_session()) {
                        // The agent's own report wins: an update naming another session is
                        // refused as a whole, its metadata (a wrong session's title) too.
                        (Some(s), Some(a)) if s != a => Err(format!(
                            "cannot link this chat to another session: its agent reported session {a}"
                        )),
                        _ => {
                            let (spec, meta) = t.updated(session_id, meta);
                            d.apply_record(&reg, t, spec, meta).map(|()| Value::Null)
                        }
                    },
                };
                drop(reg);
                reply(&self.ob, id, r);
            }
            ClientMsg::TermRelaunch {
                terminal,
                skip_permissions,
            } => relaunch::start(&d, &self.ob, id, terminal, skip_permissions, self.role),
            // Not typing: no burst of output pacing, no size claim, nothing touched.
            ClientMsg::TermAnswer {
                terminal,
                prompt,
                option,
            } => {
                let r = self
                    .terminal(&terminal)
                    .and_then(|t| t.answer(&d, prompt, option))
                    .map(|()| Value::Null);
                reply(&self.ob, id, r);
            }
            // A reply from the Chat View. Not typing at the Terminal View: no size claim, no
            // burst of output pacing, nothing to release on disconnect. Answered by the input
            // thread once typed, unless refused here.
            ClientMsg::TermSubmit {
                terminal,
                text,
                files,
            } => {
                // Everything is checked before anything is queued: the text, the Terminal
                // (visible to this connection), then the files.
                let r = submit_reply(&text, files.len())
                    .map_err(|e| e.to_string())
                    .and_then(|text| {
                        let t = self.terminal(&terminal)?;
                        // Checked and held in one step: the sweep keeps them from now until
                        // the grace after the outcome.
                        let (paths, held) = d.drops.reserve(&d.ctx, &files)?;
                        let typing = terminal::Typing::reply(&paths, text.as_deref());
                        let ob = self.ob.clone();
                        let done = Box::new(move |r: Result<(), String>| {
                            reply(&ob, id, r.map(|()| Value::Null));
                            drop(held);
                        });
                        t.submit(&d, typing, done)
                    });
                if let Err(e) = r {
                    reply(&self.ob, id, Err(e));
                }
            }
            // From agent hooks on this Host; role::check refuses it for a Mobile (#6).
            ClientMsg::TermEvent {
                terminal,
                run,
                status,
                session_id,
            } => agent::on_event(&d, &self.ob, id, terminal, run, status, session_id),
            // xshell on this machine brings its own Daemon; one replaced from elsewhere
            // would only be started again in the old version.
            ClientMsg::DaemonUpgrade if d.cfg.gui_bound.is_some() => {
                reply(&self.ob, id, Err(UPGRADE_REFUSED.to_string()))
            }
            ClientMsg::RingIdentity => reply(&self.ob, id, d.ring.identity()),
            // Never blocks: leaving another Ring says goodbye on a thread of its own.
            ClientMsg::RingJoin { rosters, expect } => {
                reply(&self.ob, id, d.ring.join(&rosters, expect.as_ref()))
            }
            // Only from a Mobile's session: the registration is bound to its Roster entry.
            ClientMsg::PushRegister {
                blob,
                seal_key,
                triggers,
            } => {
                let peer = self.peer.as_ref().filter(|_| self.role == Role::Mobile);
                let r = d.push.register(peer, &blob, &seal_key, triggers);
                reply(&self.ob, id, r)
            }
            // Answered by the session-stream worker.
            ClientMsg::SessionSubscribe { terminal, limit } => d
                .session_streams
                .subscribe(self.id, self.role, &self.ob, id, terminal, limit),
            ClientMsg::SessionPage {
                terminal,
                gen,
                before,
                limit,
            } => d.session_streams.page(
                self.id, self.role, &self.ob, id, terminal, gen, before, limit,
            ),
            ClientMsg::SessionUnsubscribe { terminal } => d
                .session_streams
                .unsubscribe(self.id, self.role, &self.ob, id, terminal),
            ClientMsg::PushUnregister => {
                let peer = self.peer.as_ref().filter(|_| self.role == Role::Mobile);
                reply(&self.ob, id, d.push.unregister(peer))
            }
            ClientMsg::DaemonUpgrade => {
                {
                    let mut reg = d.reg.lock().unwrap();
                    if !reg.frozen {
                        // The last word on disk before the Terminals are ended.
                        d.persist(&reg);
                        reg.frozen = true;
                    }
                }
                reply(&self.ob, id, Ok(Value::Null));
                let d2 = d.clone();
                let _ = std::thread::Builder::new()
                    .name("upgrade".into())
                    .spawn(move || d2.exit(ExitReason::Upgrade));
            }
        }
    }
}
