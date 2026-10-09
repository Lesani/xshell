//! Ending processes a previous `serve` left behind (it was SIGKILLed or crashed), before
//! their Terminals are relaunched, so an agent never runs twice.
//!
//! Nothing is signalled unless it is confirmed to be ours: a recorded pid alone may have been
//! reused by an unrelated process. When cleanup cannot establish that nothing of ours is left,
//! it reports [`Cleanup::Unresolved`] and the Terminal is not relaunched.

use std::time::{Duration, Instant};
use xshell_core::terminal::state::{Leader, ProcIdentity};

/// After SIGKILL, how long to wait for leftovers to disappear before giving up.
const KILL_WAIT: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(20);

/// The outcome of ending a Terminal's leftovers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cleanup {
    /// Nothing of the Terminal's previous run is alive (or it never was).
    Empty,
    /// Something may still be running and could not be ended or confirmed: relaunching
    /// would risk a second instance of the agent.
    Unresolved,
}

/// The fields of `/proc/<pid>/stat` after the command name: index 0 is field 3 (state).
#[cfg(target_os = "linux")]
pub(crate) fn stat_fields(pid: i32) -> Option<Vec<String>> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &s[s.rfind(')')? + 1..];
    Some(rest.split_whitespace().map(str::to_string).collect())
}

/// A process's start time, to tell a reused pid from the original. Linux: field 22 of
/// `/proc/<pid>/stat`; macOS: `proc_pidinfo` start time in microseconds. `None` if the
/// process does not exist (or is a zombie) or the platform offers no way to read it.
#[cfg(target_os = "linux")]
pub(crate) fn start_time(pid: i32) -> Option<u64> {
    let f = stat_fields(pid)?;
    if f.first().map(String::as_str) == Some("Z") {
        return None;
    }
    f.get(19)?.parse().ok()
}

#[cfg(target_os = "macos")]
fn bsdinfo(pid: i32) -> Option<libc::proc_bsdinfo> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        )
    };
    (n == size).then_some(info)
}

#[cfg(target_os = "macos")]
pub(crate) fn start_time(pid: i32) -> Option<u64> {
    let i = bsdinfo(pid)?;
    // SZOMB = 5: a zombie is dead for our purposes.
    if i.pbi_status == 5 {
        return None;
    }
    Some(i.pbi_start_tvsec * 1_000_000 + i.pbi_start_tvusec)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn start_time(_pid: i32) -> Option<u64> {
    None
}

/// The identity of `pid` right now.
pub(crate) fn identity(pid: i32) -> ProcIdentity {
    ProcIdentity {
        pid,
        start_time: start_time(pid),
    }
}

/// `who` is still the very process that was recorded.
fn confirmed(who: &ProcIdentity) -> bool {
    who.pid > 1 && who.start_time.is_some() && start_time(who.pid) == who.start_time
}

// ── Linux: the leader's session, signalled through pidfds ──────────────────

/// Live (non-zombie) processes in session `sid`, except ourselves.
#[cfg(target_os = "linux")]
fn session_members(sid: i32) -> Vec<i32> {
    let me = std::process::id() as i32;
    let Ok(rd) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    rd.flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<i32>().ok())
        .filter(|&p| p != me && in_session(p, sid))
        .collect()
}

/// `pid` is alive (not a zombie) and in session `sid`.
#[cfg(target_os = "linux")]
fn in_session(pid: i32, sid: i32) -> bool {
    stat_fields(pid).is_some_and(|f| {
        f.first().map(String::as_str) != Some("Z")
            && f.get(3).and_then(|s| s.parse::<i32>().ok()) == Some(sid)
    })
}

/// What happened to one signal.
#[cfg(target_os = "linux")]
#[derive(Debug, PartialEq, Eq)]
enum Sent {
    Signalled,
    /// The pid no longer names a member of the session (it exited, possibly reused).
    NotMember,
    /// pidfds are unavailable (kernel < 5.3); the process was left alone.
    Unsupported,
}

/// Signal `pid` only if it is a member of `sid`, pinned by a pidfd: the pidfd is opened
/// first and membership is checked after, so the signal reaches the process that was
/// checked even if the number is reused in between. Never falls back to a bare `kill`.
#[cfg(target_os = "linux")]
fn signal_member(pid: i32, sid: i32, sig: i32) -> Sent {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
    if fd < 0 {
        return match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::ENOSYS) => Sent::Unsupported,
            _ => Sent::NotMember,
        };
    }
    let sent = if in_session(pid, sid) {
        let r = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                fd,
                sig,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        if r == 0 {
            Sent::Signalled
        } else {
            Sent::NotMember
        }
    } else {
        Sent::NotMember
    };
    unsafe { libc::close(fd) };
    sent
}

/// The Linux escalation over an injectable session scan (tests substitute it).
#[cfg(target_os = "linux")]
fn escalate_session(
    lead: &ProcIdentity,
    grace: Duration,
    scan: &mut dyn FnMut() -> Vec<i32>,
) -> Cleanup {
    use std::collections::HashSet;
    let sid = lead.pid;
    let hup_until = Instant::now() + grace;
    let give_up = hup_until + KILL_WAIT;
    let mut hupped: HashSet<i32> = HashSet::new();
    loop {
        // A live process numbered like the session with another start time means our
        // session ended and the number now belongs to someone else.
        if start_time(sid).is_some() && !confirmed(lead) {
            return Cleanup::Empty;
        }
        let members = scan();
        if members.is_empty() {
            return Cleanup::Empty;
        }
        let now = Instant::now();
        if now >= give_up {
            crate::log!(
                "WARN",
                "{} leftover process(es) of session {sid} could not be ended",
                members.len()
            );
            return Cleanup::Unresolved;
        }
        for p in members {
            // HUP once per process; KILL on every round until it is gone.
            let sig = if now < hup_until {
                if !hupped.insert(p) {
                    continue;
                }
                libc::SIGHUP
            } else {
                libc::SIGKILL
            };
            if signal_member(p, sid, sig) == Sent::Unsupported {
                crate::log!("WARN", "pidfds unavailable; not signalling leftover {p}");
            }
        }
        std::thread::sleep(POLL);
    }
}

/// End what is left of a Terminal's previous process tree: SIGHUP, then SIGKILL after
/// `grace`, rescanning the session every round (a process forked while handling the hangup
/// is caught too), until no member is left or a bounded wait runs out.
///
/// The session is adopted only if its recorded leader is alive with the recorded start
/// time. A live leader with another start time means the pid was reused, so our session is
/// long gone. A dead leader cannot be confirmed: if processes still carry its session id,
/// they may be ours or a stranger's (the id may have been reused), so they are left alone
/// and the outcome is unresolved.
#[cfg(target_os = "linux")]
pub(crate) fn end_leftovers(leader: &Leader, grace: Duration) -> Cleanup {
    let sid = leader.pid as i32;
    if sid <= 1 {
        return Cleanup::Empty;
    }
    let lead = ProcIdentity {
        pid: sid,
        start_time: leader.start_time,
    };
    match start_time(sid) {
        Some(_) if confirmed(&lead) => escalate_session(&lead, grace, &mut || session_members(sid)),
        Some(_) if lead.start_time.is_some() => Cleanup::Empty,
        _ if session_members(sid).is_empty() => Cleanup::Empty,
        _ => {
            crate::log!(
                "WARN",
                "session {sid} has members but its leader cannot be confirmed; leaving it alone"
            );
            Cleanup::Unresolved
        }
    }
}

// ── macOS: the recorded process groups, revalidated every round ────────────

/// `g` is still the recorded process and still leads its own group.
#[cfg(target_os = "macos")]
fn leads_group(g: &ProcIdentity) -> bool {
    confirmed(g) && bsdinfo(g.pid).is_some_and(|i| i.pbi_pgid as i32 == g.pid)
}

#[cfg(target_os = "macos")]
fn group_exists(g: i32) -> bool {
    let r = unsafe { libc::killpg(g, 0) };
    r == 0
}

/// macOS: the recorded process groups whose leader is still the recorded process and
/// still leads the group. Identity and membership are rechecked before every signal; a
/// group that fails the check once is never signalled again. A group that still exists
/// without a confirmable leader leaves the outcome unresolved.
#[cfg(target_os = "macos")]
pub(crate) fn end_leftovers(leader: &Leader, grace: Duration) -> Cleanup {
    let mut groups: Vec<ProcIdentity> = leader.groups.clone();
    let lead = ProcIdentity {
        pid: leader.pid as i32,
        start_time: leader.start_time,
    };
    if !groups.iter().any(|g| g.pid == lead.pid) {
        groups.push(lead);
    }
    groups.retain(|g| g.pid > 1);
    // Groups whose leader is dead but which may still hold members of ours.
    let mut unconfirmed: Vec<i32> = Vec::new();
    let mut active: Vec<ProcIdentity> = Vec::new();
    for g in groups {
        if leads_group(&g) {
            active.push(g);
        } else if start_time(g.pid).is_none() && group_exists(g.pid) {
            unconfirmed.push(g.pid);
        }
        // Else the number was reused: our group is gone.
    }
    let hup_until = Instant::now() + grace;
    let give_up = hup_until + KILL_WAIT;
    let mut hupped = false;
    loop {
        // Revalidate before each round; drop a group for good once it fails.
        let mut still = Vec::new();
        for g in active {
            if leads_group(&g) {
                still.push(g);
            } else if group_exists(g.pid) {
                // Its leader died but members remain: no longer confirmable.
                unconfirmed.push(g.pid);
            }
        }
        active = still;
        if active.is_empty() {
            break;
        }
        let now = Instant::now();
        if now >= give_up {
            crate::log!(
                "WARN",
                "{} leftover process group(s) survived SIGKILL",
                active.len()
            );
            return Cleanup::Unresolved;
        }
        if now < hup_until {
            if !hupped {
                for g in &active {
                    unsafe { libc::killpg(g.pid, libc::SIGHUP) };
                }
                hupped = true;
            }
        } else {
            for g in &active {
                unsafe { libc::killpg(g.pid, libc::SIGKILL) };
            }
        }
        std::thread::sleep(POLL);
    }
    unconfirmed.retain(|&g| group_exists(g));
    if unconfirmed.is_empty() {
        Cleanup::Empty
    } else {
        crate::log!(
            "WARN",
            "process group(s) {unconfirmed:?} remain without a confirmable leader; leaving them alone"
        );
        Cleanup::Unresolved
    }
}

/// Elsewhere there is no way to confirm a process's identity: nothing is signalled, and a
/// recorded leader that may still be running leaves the outcome unresolved.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn end_leftovers(leader: &Leader, _grace: Duration) -> Cleanup {
    let _ = confirmed;
    if unsafe { libc::kill(leader.pid as i32, 0) } == 0 {
        Cleanup::Unresolved
    } else {
        Cleanup::Empty
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command};

    /// `program args` leading its own session (and process group), like a Terminal's leader.
    fn session_leader(program: &str, args: &[&str]) -> Child {
        let mut c = Command::new(program);
        c.args(args);
        unsafe {
            c.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        c.spawn().unwrap()
    }

    fn reaped(c: &mut Child, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if c.try_wait().unwrap().is_some() {
                return true;
            }
            std::thread::sleep(POLL);
        }
        false
    }

    fn leader_of(pid: i32, start_time: Option<u64>) -> Leader {
        Leader {
            pid: pid as u32,
            start_time,
            groups: vec![ProcIdentity { pid, start_time }],
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn ends_a_confirmed_leftover() {
        let mut c = session_leader("sleep", &["1000"]);
        let pid = c.id() as i32;
        let st = start_time(pid);
        assert!(st.is_some());
        assert_eq!(
            end_leftovers(&leader_of(pid, st), Duration::from_millis(200)),
            Cleanup::Empty
        );
        assert!(reaped(&mut c, Duration::from_secs(3)));
    }

    /// A live process whose start time differs is a reused pid: never signalled, and our
    /// session is gone.
    #[test]
    fn never_signals_an_unconfirmed_pid() {
        let mut c = session_leader("sleep", &["1000"]);
        let pid = c.id() as i32;
        let wrong = start_time(pid).map(|t| t + 1).or(Some(1));
        let r = end_leftovers(&leader_of(pid, wrong), Duration::from_millis(100));
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            assert_eq!(r, Cleanup::Empty);
        }
        assert!(
            c.try_wait().unwrap().is_none(),
            "an unconfirmed process was signalled"
        );
        c.kill().unwrap();
        c.wait().unwrap();
    }

    /// A session whose leader has exited cannot be confirmed: its surviving members are
    /// left alone and the outcome is unresolved.
    #[cfg(target_os = "linux")]
    #[test]
    fn leaderless_session_is_not_adopted() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("kid");
        let script = format!("sleep 1000 & echo $! > '{}'", pidfile.display());
        let mut lead = session_leader("sh", &["-c", &script]);
        let sid = lead.id() as i32;
        let st = start_time(sid);
        lead.wait().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let kid: i32 = loop {
            if let Some(k) = std::fs::read_to_string(&pidfile)
                .ok()
                .and_then(|s| s.trim().parse().ok())
            {
                break k;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(POLL);
        };
        assert!(in_session(kid, sid));
        let r = end_leftovers(&leader_of(sid, st), Duration::from_millis(100));
        let survived = in_session(kid, sid);
        unsafe { libc::kill(kid, libc::SIGKILL) };
        assert_eq!(r, Cleanup::Unresolved);
        assert!(survived, "a member of an unconfirmed session was signalled");
    }

    /// A scanned pid that is no longer a member when it is signalled (it exited and the
    /// number now names another process) is never signalled, and keeps the outcome
    /// unresolved while the scan reports it.
    #[cfg(target_os = "linux")]
    #[test]
    fn pid_reused_between_scan_and_signal_is_spared() {
        // The stranger: a process outside the session the scan claims it is in.
        let mut stranger = Command::new("sleep").arg("1000").spawn().unwrap();
        let spid = stranger.id() as i32;
        let mut lead = session_leader("sleep", &["1000"]);
        let lead_id = identity(lead.id() as i32);
        assert_eq!(
            signal_member(spid, lead_id.pid, libc::SIGKILL),
            Sent::NotMember
        );
        let r = escalate_session(&lead_id, Duration::from_millis(50), &mut || vec![spid]);
        assert_eq!(r, Cleanup::Unresolved);
        assert!(
            stranger.try_wait().unwrap().is_none(),
            "the stranger was signalled"
        );
        for c in [&mut stranger, &mut lead] {
            c.kill().unwrap();
            c.wait().unwrap();
        }
    }

    #[test]
    fn identity_tracks_start_time() {
        let me = identity(std::process::id() as i32);
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            assert!(confirmed(&me));
        }
        assert!(!confirmed(&ProcIdentity {
            pid: me.pid,
            start_time: None
        }));
    }
}
