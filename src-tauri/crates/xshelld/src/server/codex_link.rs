//! The Daemon's own link of a Codex Terminal to its session. Codex starts without a session
//! id; its `notify` hook names the session (`thread-id`) at the end of every turn, and
//! `term.event` carries it here. The last-line worker checks it off the connection threads
//! and links the Terminal through the same path as a `term.update`.
//!
//! The check: a valid id of at most 200 characters, a Codex Terminal, a rollout for the id
//! that is a regular file inside `~/.codex/sessions`, whose first line is the `session_meta`
//! of that id, in the Terminal's working directory and not a subagent's thread. Any process
//! of the user may send a `term.event`: the check makes sure the session is one Codex keeps,
//! in this Terminal's directory, never that this Terminal's Codex reported it.
//!
//! The agent's own report wins: once linked, a `term.update` naming another session is
//! refused (`conn.rs`).

use super::registry::Daemon;
use super::terminal::{PendingLink, Terminal};
use super::TestPoint;
use std::sync::Arc;
use std::time::Instant;
use xshell_core::agent_status::{hook_agent, HookAgent};
use xshell_core::chat::SESSION_ID_MAX;
use xshell_core::codex::{rollout_for_within, session_meta_in, sessions_root};
use xshell_core::launch::LaunchSpec;
use xshell_core::sessions::valid_session_id;

/// The most directory entries one look for a rollout visits.
const WALK_MAX: usize = 100_000;

/// Why a reported session is not linked.
#[derive(Debug)]
enum Refused {
    /// No rollout for it yet: Codex may not have written it. Looked for again until the retry.
    Missing,
    /// Anything else, with the reason.
    Invalid(String),
}

/// Check `t`'s waiting session report and link it, or keep it for the retry, or drop it.
/// Called by the last-line worker before it reads the line, with no lock held.
pub(crate) fn resolve(d: &Daemon, t: &Arc<Terminal>) {
    // (a) The report to check, for the listed Terminal of a Codex agent.
    let (p, spec) = {
        let reg = d.reg.lock().unwrap();
        if !d.is_current(&reg, t) {
            return;
        }
        let Some(p) = t.pending_link() else {
            return;
        };
        let spec = t.spec();
        if hook_agent(&spec) != Some(HookAgent::Codex) {
            t.drop_link(p.gen);
            return;
        }
        (p, spec)
    };
    // (b) The check, without a lock: it reads the session storage.
    let checked = check(d, &p.sid, &spec);
    d.test_point(t.id, TestPoint::LinkChecked);
    decide(d, t, &p, &spec, checked);
    d.test_point(t.id, TestPoint::LinkDone);
}

/// (c) Link report `p`, keep it for the retry or drop it, under the registry lock: only for
/// the same Terminal, report, agent and directory it was checked for (`spec`).
fn decide(
    d: &Daemon,
    t: &Arc<Terminal>,
    p: &PendingLink,
    spec: &LaunchSpec,
    checked: Result<(), Refused>,
) {
    let reg = d.reg.lock().unwrap();
    if !d.is_current(&reg, t) || t.pending_link().as_ref() != Some(p) {
        return;
    }
    let current = t.spec();
    if hook_agent(&current) != Some(HookAgent::Codex) || current.cwd != spec.cwd {
        t.drop_link(p.gen);
        return;
    }
    match checked {
        Ok(()) => {
            if current.session_id.as_deref() == Some(p.sid.as_str()) {
                t.commit_link(p.gen);
                return;
            }
            let (next, meta) = t.updated(Some(p.sid.clone()), None);
            match d.apply_record(&reg, t, next, meta) {
                // Only a link that took is the agent's session.
                Ok(()) => {
                    t.commit_link(p.gen);
                    crate::log!(
                        "INFO",
                        "terminal {} linked to Codex session {}",
                        t.id,
                        p.sid
                    );
                }
                Err(e) => {
                    t.drop_link(p.gen);
                    crate::log!(
                        "WARN",
                        "terminal {}: not linking Codex session {}: {e}",
                        t.id,
                        p.sid
                    );
                }
            }
        }
        Err(Refused::Missing) if Instant::now() < p.at + d.cfg.last_line_retry => {}
        Err(e) => {
            t.drop_link(p.gen);
            let why = match e {
                Refused::Missing => "no rollout for it".to_string(),
                Refused::Invalid(why) => why,
            };
            crate::log!(
                "WARN",
                "terminal {}: not linking Codex session {:?}: {why}",
                t.id,
                shown(&p.sid)
            );
        }
    }
}

/// Whether `sid` may be `spec`'s session: see the module documentation.
fn check(d: &Daemon, sid: &str, spec: &LaunchSpec) -> Result<(), Refused> {
    if sid.len() > SESSION_ID_MAX || !valid_session_id(sid) {
        return Err(Refused::Invalid("not a valid session id".into()));
    }
    let root = sessions_root(&d.ctx).ok_or(Refused::Invalid("no home directory".into()))?;
    let path = rollout_for_within(&d.ctx, sid, WALK_MAX).ok_or(Refused::Missing)?;
    let meta = session_meta_in(&root, &path).ok_or_else(|| {
        Refused::Invalid("its rollout is not a readable session file inside storage".into())
    })?;
    if meta.id != sid {
        return Err(Refused::Invalid(format!(
            "its rollout names session {:?}",
            meta.id
        )));
    }
    if !same_dir(&meta.cwd, &spec.cwd) {
        return Err(Refused::Invalid(format!(
            "it runs in {:?}, the terminal in {:?}",
            meta.cwd, spec.cwd
        )));
    }
    if meta.subagent {
        return Err(Refused::Invalid("it is a subagent's thread".into()));
    }
    Ok(())
}

/// `sid` for the log: a report may carry anything, the log gets at most a session id's
/// length of it.
fn shown(sid: &str) -> String {
    sid.chars().take(SESSION_ID_MAX).collect()
}

/// The same directory: the same path, or the same real path.
fn same_dir(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}
