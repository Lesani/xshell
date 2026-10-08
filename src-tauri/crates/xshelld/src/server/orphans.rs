//! Ending processes a previous `serve` left behind (it was SIGKILLed or crashed), before
//! their Terminals are relaunched, so an agent never runs twice. Nothing is signalled unless
//! it can be confirmed to be ours: a pid alone may have been reused by an unrelated process.

use std::time::{Duration, Instant};
use xshell_core::terminal::state::{Leader, ProcIdentity};

/// After SIGKILL, how long to wait for leftovers to disappear before giving up.
const KILL_WAIT: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(20);

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

/// Live (non-zombie) processes in session `sid`, except ourselves.
#[cfg(target_os = "linux")]
fn session_members(sid: i32) -> Vec<i32> {
    let me = std::process::id() as i32;
    let Ok(rd) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    rd.flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<i32>().ok())
        .filter(|&p| p != me)
        .filter(|&p| {
            stat_fields(p).is_some_and(|f| {
                f.first().map(String::as_str) != Some("Z")
                    && f.get(3).and_then(|s| s.parse::<i32>().ok()) == Some(sid)
            })
        })
        .collect()
}

/// End what is left of a Terminal's previous process tree: SIGHUP, then SIGKILL after
/// `grace`, until nothing is left (bounded). Returns how many processes or groups were
/// signalled.
///
/// Linux: every member of the leader's session, rescanned on each round, so a process
/// forked while handling the hangup is caught too. The session is ours if its leader is
/// gone (a session id cannot be reused while any member exists) or still has the recorded
/// start time; a live leader we cannot confirm means the pid was reused, and nothing is done.
#[cfg(target_os = "linux")]
pub(crate) fn end_leftovers(leader: &Leader, grace: Duration) -> usize {
    use std::collections::HashSet;
    let sid = leader.pid as i32;
    if sid <= 1 {
        return 0;
    }
    let lead = ProcIdentity {
        pid: sid,
        start_time: leader.start_time,
    };
    let ours = || start_time(sid).is_none() || confirmed(&lead);
    let hup_until = Instant::now() + grace;
    let give_up = hup_until + KILL_WAIT;
    let mut signalled: HashSet<i32> = HashSet::new();
    loop {
        if !ours() {
            return signalled.len();
        }
        let members = session_members(sid);
        if members.is_empty() {
            return signalled.len();
        }
        let now = Instant::now();
        if now >= give_up {
            crate::log!(
                "WARN",
                "{} leftover process(es) of session {sid} survived SIGKILL",
                members.len()
            );
            return signalled.len();
        }
        let sig = if now < hup_until {
            libc::SIGHUP
        } else {
            libc::SIGKILL
        };
        for p in members {
            // HUP once per process; KILL on every round until it is gone.
            if sig == libc::SIGKILL || signalled.insert(p) {
                signalled.insert(p);
                unsafe { libc::kill(p, sig) };
            }
        }
        std::thread::sleep(POLL);
    }
}

/// macOS: the recorded process groups whose group leader is still the recorded process
/// (same start time, still leading its group). A group whose leader has exited cannot be
/// confirmed and is left alone; the closed terminal already sent it SIGHUP.
#[cfg(target_os = "macos")]
pub(crate) fn end_leftovers(leader: &Leader, grace: Duration) -> usize {
    let mut groups: Vec<ProcIdentity> = leader.groups.clone();
    let lead = ProcIdentity {
        pid: leader.pid as i32,
        start_time: leader.start_time,
    };
    if !groups.iter().any(|g| g.pid == lead.pid) {
        groups.push(lead);
    }
    groups.retain(|g| confirmed(g) && bsdinfo(g.pid).is_some_and(|i| i.pbi_pgid as i32 == g.pid));
    let exists = |g: i32| unsafe { libc::killpg(g, 0) } == 0;
    for g in &groups {
        unsafe { libc::killpg(g.pid, libc::SIGHUP) };
    }
    let hup_until = Instant::now() + grace;
    while Instant::now() < hup_until && groups.iter().any(|g| exists(g.pid)) {
        std::thread::sleep(POLL);
    }
    let give_up = Instant::now() + KILL_WAIT;
    while Instant::now() < give_up && groups.iter().any(|g| exists(g.pid)) {
        for g in &groups {
            if exists(g.pid) {
                unsafe { libc::killpg(g.pid, libc::SIGKILL) };
            }
        }
        std::thread::sleep(POLL);
    }
    groups.len()
}

/// Elsewhere there is no way to confirm a process's identity, so nothing is signalled.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn end_leftovers(leader: &Leader, _grace: Duration) -> usize {
    let _ = (leader, confirmed);
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command};

    /// A `sleep` that leads its own session (and process group), like a Terminal's leader.
    fn session_leader() -> Child {
        let mut c = Command::new("sleep");
        c.arg("1000");
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
        let mut c = session_leader();
        let pid = c.id() as i32;
        let st = start_time(pid);
        assert!(st.is_some());
        assert!(end_leftovers(&leader_of(pid, st), Duration::from_millis(200)) >= 1);
        assert!(reaped(&mut c, Duration::from_secs(3)));
    }

    /// A live process whose start time differs is a reused pid: never signalled.
    #[test]
    fn never_signals_an_unconfirmed_pid() {
        let mut c = session_leader();
        let pid = c.id() as i32;
        let wrong = start_time(pid).map(|t| t + 1).or(Some(1));
        assert_eq!(
            end_leftovers(&leader_of(pid, wrong), Duration::from_millis(100)),
            0
        );
        assert!(
            c.try_wait().unwrap().is_none(),
            "an unconfirmed process was signalled"
        );
        c.kill().unwrap();
        c.wait().unwrap();
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
