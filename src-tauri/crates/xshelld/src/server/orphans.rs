//! Ending processes a previous `serve` left behind (it was SIGKILLed or crashed), before
//! their Terminals are relaunched, so an agent never runs twice.

use std::time::{Duration, Instant};
use xshell_core::terminal::state::Leader;

/// The fields of `/proc/<pid>/stat` after the command name: index 0 is field 3 (state).
#[cfg(target_os = "linux")]
pub(crate) fn stat_fields(pid: i32) -> Option<Vec<String>> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &s[s.rfind(')')? + 1..];
    Some(rest.split_whitespace().map(str::to_string).collect())
}

#[cfg(target_os = "linux")]
fn session_of(pid: i32) -> Option<i32> {
    stat_fields(pid)?.get(3)?.parse().ok()
}

/// Every process in session `sid` except ourselves.
#[cfg(target_os = "linux")]
fn session_members(sid: i32) -> Vec<i32> {
    let me = std::process::id() as i32;
    let Ok(rd) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    rd.flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<i32>().ok())
        .filter(|&p| p != me && session_of(p) == Some(sid))
        .collect()
}

#[cfg(target_os = "linux")]
fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

/// End what is left of a Terminal's previous process tree: SIGHUP, then SIGKILL after
/// `grace`. Returns how many processes (Linux) or groups (elsewhere) were signalled.
///
/// Linux: the members of the leader's session. A live leader whose start time differs is a
/// reused pid, and its session is not ours. While any member exists the session id cannot be
/// reused, so members found by session id are ours.
#[cfg(target_os = "linux")]
pub(crate) fn end_leftovers(leader: &Leader, grace: Duration) -> usize {
    let sid = leader.pid as i32;
    if sid <= 1 {
        return 0;
    }
    if let (Some(want), Some(f)) = (leader.start_time, stat_fields(sid)) {
        if f.get(19).and_then(|s| s.parse::<u64>().ok()) != Some(want) {
            return 0;
        }
    }
    let members = session_members(sid);
    for &p in &members {
        unsafe { libc::kill(p, libc::SIGHUP) };
    }
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline && members.iter().any(|&p| session_of(p) == Some(sid)) {
        std::thread::sleep(Duration::from_millis(20));
    }
    for &p in &members {
        if session_of(p) == Some(sid) && alive(p) {
            unsafe { libc::kill(p, libc::SIGKILL) };
        }
    }
    members.len()
}

/// Elsewhere: the persisted process groups that still exist.
#[cfg(not(target_os = "linux"))]
pub(crate) fn end_leftovers(leader: &Leader, grace: Duration) -> usize {
    let mut groups: Vec<i32> = leader.pgids.clone();
    if !groups.contains(&(leader.pid as i32)) {
        groups.push(leader.pid as i32);
    }
    groups.retain(|&g| g > 1 && unsafe { libc::killpg(g, 0) } == 0);
    for &g in &groups {
        unsafe { libc::killpg(g, libc::SIGHUP) };
    }
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline && groups.iter().any(|&g| unsafe { libc::killpg(g, 0) } == 0) {
        std::thread::sleep(Duration::from_millis(20));
    }
    for &g in &groups {
        if unsafe { libc::killpg(g, 0) } == 0 {
            unsafe { libc::killpg(g, libc::SIGKILL) };
        }
    }
    groups.len()
}
