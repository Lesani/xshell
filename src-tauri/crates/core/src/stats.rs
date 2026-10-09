use crate::ctx::HostCtx;
use crate::time::system_time_to_iso;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::time::SystemTime;

// ── xshell-stats integration ──────────────────────────────────────────
// Reads pre-computed session stats produced by a Claude Code statusLine hook (the user's
// own script writes them to ~/.claude/xshell-stats/<session_id>.json). When present, this
// gives us authoritative cost / context-% / rate-limits straight from Claude Code instead
// of our JSONL-derived estimates.

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct StatuslineProbe {
    // Whether the user has any statusLine configured at all in ~/.claude/settings.json.
    pub has_statusline: bool,
    pub existing_command: Option<String>,
    // Whether xshell-stats/ exists and has at least one session file (proxy for "is the
    // hook script actually running?"). When `has_statusline` is true but this is false,
    // the user likely needs to merge our snippet into their existing script.
    pub stats_dir_present: bool,
    pub stats_session_count: usize,
    // Most recent file mtime in xshell-stats/ as ISO-8601, for the "last update X ago" UI.
    pub last_update_iso: Option<String>,
    pub home_dir: String,
    pub stats_dir_path: String,
}

pub fn probe_statusline_setup(ctx: &HostCtx) -> StatuslineProbe {
    let home = ctx.home.clone().unwrap_or_default();
    let stats_dir = home.join(".claude").join("xshell-stats");
    let settings_path = home.join(".claude").join("settings.json");

    let mut has_statusline = false;
    let mut existing_command: Option<String> = None;
    if let Ok(content) = fs::read_to_string(&settings_path) {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
            if let Some(sl) = json.get("statusLine") {
                has_statusline = true;
                existing_command = sl
                    .get("command")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
            }
        }
    }

    let mut stats_session_count = 0usize;
    let mut last_modified: Option<SystemTime> = None;
    if stats_dir.exists() {
        for entry in fs::read_dir(&stats_dir)
            .ok()
            .into_iter()
            .flatten()
            .flatten()
        {
            if entry.file_type().is_ok_and(|ft| ft.is_file()) {
                stats_session_count += 1;
                if let Ok(meta) = entry.metadata() {
                    if let Ok(modified) = meta.modified() {
                        if last_modified.is_none_or(|m| modified > m) {
                            last_modified = Some(modified);
                        }
                    }
                }
            }
        }
    }

    StatuslineProbe {
        has_statusline,
        existing_command,
        stats_dir_present: stats_dir.exists(),
        stats_session_count,
        last_update_iso: last_modified.map(system_time_to_iso),
        home_dir: home.to_string_lossy().into_owned(),
        stats_dir_path: stats_dir.to_string_lossy().into_owned(),
    }
}

// Global rate-limit snapshot. The numbers are account-wide (not per-session) — Claude Code
// reports the same 5h/7d percentages on every session's statusline at any given moment. We
// pick the freshest stats file as the source of truth and surface it once, in the sidebar.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct GlobalRateLimits {
    pub five_hour_pct: Option<f64>,
    pub seven_day_pct: Option<f64>,
    pub five_hour_resets_at: Option<u64>, // unix seconds
    pub seven_day_resets_at: Option<u64>,
    pub last_update_iso: Option<String>,
    pub source_session_id: Option<String>,
}

pub fn get_global_rate_limits(ctx: &HostCtx) -> GlobalRateLimits {
    let mut out = GlobalRateLimits {
        five_hour_pct: None,
        seven_day_pct: None,
        five_hour_resets_at: None,
        seven_day_resets_at: None,
        last_update_iso: None,
        source_session_id: None,
    };
    let Some(home) = ctx.home.clone() else {
        return out;
    };
    let stats_dir = home.join(".claude").join("xshell-stats");
    if !stats_dir.exists() {
        return out;
    }

    // Collect files newest-first by mtime. We can't just read the single freshest file:
    // Claude Code omits the `rate_limits` block from ~half of its statusline ticks (e.g. a
    // session's early ticks before the first API response carries rate-limit headers), and
    // the hook overwrites the whole payload each tick. So the newest file often lacks rate
    // limits even when older files hold a valid snapshot. Walk newest→oldest and take the
    // first file that actually has rate-limit data — last-known-good beats a blank chip,
    // and rate limits are account-wide + slow-moving so a slightly older snapshot is fine.
    let mut files: Vec<(SystemTime, std::path::PathBuf)> = fs::read_dir(&stats_dir)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|ft| ft.is_file()))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    files.sort_by_key(|f| std::cmp::Reverse(f.0));

    for (mtime, path) in &files {
        let Ok(content) = fs::read_to_string(path) else {
            continue;
        };
        let Ok(json): Result<serde_json::Value, _> = serde_json::from_str(&content) else {
            continue;
        };
        let Some(rl) = json.get("rate_limits") else {
            continue;
        };
        let five = rl.get("five_hour");
        let seven = rl.get("seven_day");
        // Require at least one window's percentage to consider this a usable snapshot.
        let five_pct = five
            .and_then(|w| w.get("used_percentage"))
            .and_then(|v| v.as_f64());
        let seven_pct = seven
            .and_then(|w| w.get("used_percentage"))
            .and_then(|v| v.as_f64());
        if five_pct.is_none() && seven_pct.is_none() {
            continue;
        }

        out.five_hour_pct = five_pct;
        out.five_hour_resets_at = five
            .and_then(|w| w.get("resets_at"))
            .and_then(|v| v.as_u64());
        out.seven_day_pct = seven_pct;
        out.seven_day_resets_at = seven
            .and_then(|w| w.get("resets_at"))
            .and_then(|v| v.as_u64());
        // Report the freshness of the snapshot we actually used, not the newest file overall.
        out.last_update_iso = Some(system_time_to_iso(*mtime));
        out.source_session_id = path.file_stem().map(|s| s.to_string_lossy().into_owned());
        break;
    }
    out
}

// ── Home usage strip ──────────────────────────────────────────────────
// Aggregates for the dashboard strip on the home screen. Claude cost comes from the
// xshell-stats hook files (authoritative, per-session daily maps summed across sessions);
// Codex rate limits and activity come straight from the rollout files — Codex needs no
// hook, every token_count event carries usage + rate-limit data.

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct DailyUsd {
    pub date: String, // YYYY-MM-DD as written by the hook (local date)
    pub usd: f64,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ClaudeCostSummary {
    // Whether any xshell-stats session files exist — the strip's "hook is set up" signal.
    pub connected: bool,
    pub daily: Vec<DailyUsd>, // ascending by date; today/this-week math happens client-side
}

pub fn get_claude_cost_summary(ctx: &HostCtx) -> ClaudeCostSummary {
    let Some(home) = ctx.home.clone() else {
        return ClaudeCostSummary {
            connected: false,
            daily: vec![],
        };
    };
    let stats_dir = home.join(".claude").join("xshell-stats");

    let mut connected = false;
    let mut by_date: HashMap<String, f64> = HashMap::new();
    for entry in fs::read_dir(&stats_dir)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
    {
        if !entry.file_type().is_ok_and(|ft| ft.is_file()) {
            continue;
        }
        let Ok(content) = fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) else {
            continue;
        };
        connected = true;
        if let Some(map) = json.get("xshell_daily_cost").and_then(|v| v.as_object()) {
            for (date, usd) in map {
                if let Some(u) = usd.as_f64() {
                    *by_date.entry(date.clone()).or_insert(0.0) += u;
                }
            }
        }
    }

    let mut daily: Vec<DailyUsd> = by_date
        .into_iter()
        .map(|(date, usd)| DailyUsd { date, usd })
        .collect();
    daily.sort_by(|a, b| a.date.cmp(&b.date));
    ClaudeCostSummary { connected, daily }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::testutil::Fixture;
    use serde_json::json;
    use std::time::Duration;

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn probe_statusline_setup_reports_command_and_stats() {
        let fx = Fixture::new();
        fx.write(
            "home/.claude/settings.json",
            json!({"statusLine": {"type": "command", "command": "~/bin/sl.sh"}}).to_string(),
        );
        let a = fx.write("home/.claude/xshell-stats/a.json", "{}");
        let b = fx.write("home/.claude/xshell-stats/b.json", "{}");
        fx.set_mtime(&a, at(1_700_000_000));
        fx.set_mtime(&b, at(1_700_000_500));
        let p = probe_statusline_setup(&fx.ctx());
        assert!(p.has_statusline);
        assert_eq!(p.existing_command.as_deref(), Some("~/bin/sl.sh"));
        assert!(p.stats_dir_present);
        assert_eq!(p.stats_session_count, 2);
        assert_eq!(
            p.last_update_iso,
            Some(system_time_to_iso(at(1_700_000_500)))
        );
        assert_eq!(p.home_dir, fx.home().to_string_lossy());
        assert_eq!(
            std::path::PathBuf::from(&p.stats_dir_path),
            fx.home().join(".claude").join("xshell-stats")
        );

        // Nothing set up.
        let empty = Fixture::new();
        let p = probe_statusline_setup(&empty.ctx());
        assert!(!p.has_statusline && !p.stats_dir_present);
        assert_eq!(
            (p.existing_command, p.stats_session_count, p.last_update_iso),
            (None, 0, None)
        );
    }

    #[test]
    fn get_global_rate_limits_falls_back_to_older_snapshot() {
        let fx = Fixture::new();
        let newest = fx.write(
            "home/.claude/xshell-stats/newest.json",
            json!({"cost": {"total_cost_usd": 1.0}}).to_string(),
        );
        let middle = fx.write(
            "home/.claude/xshell-stats/middle.json",
            json!({"rate_limits": {
                "five_hour": {"used_percentage": 42.5, "resets_at": 1_700_003_600u64},
                "seven_day": {"used_percentage": 10.0, "resets_at": 1_700_600_000u64}
            }})
            .to_string(),
        );
        let oldest = fx.write(
            "home/.claude/xshell-stats/oldest.json",
            json!({"rate_limits": {"five_hour": {"used_percentage": 99.0}}}).to_string(),
        );
        fx.set_mtime(&newest, at(1_700_000_300));
        fx.set_mtime(&middle, at(1_700_000_200));
        fx.set_mtime(&oldest, at(1_700_000_100));
        let r = get_global_rate_limits(&fx.ctx());
        assert_eq!(r.five_hour_pct, Some(42.5));
        assert_eq!(r.five_hour_resets_at, Some(1_700_003_600));
        assert_eq!(r.seven_day_pct, Some(10.0));
        assert_eq!(r.seven_day_resets_at, Some(1_700_600_000));
        assert_eq!(r.source_session_id.as_deref(), Some("middle"));
        assert_eq!(
            r.last_update_iso,
            Some(system_time_to_iso(at(1_700_000_200)))
        );
    }

    #[test]
    fn get_claude_cost_summary_sums_daily_costs() {
        let fx = Fixture::new();
        fx.write(
            "home/.claude/xshell-stats/a.json",
            json!({"xshell_daily_cost": {"2026-01-02": 1.5, "2026-01-01": 0.25}}).to_string(),
        );
        fx.write(
            "home/.claude/xshell-stats/b.json",
            json!({"xshell_daily_cost": {"2026-01-02": 2.25, "2026-01-03": "n/a"}}).to_string(),
        );
        fx.write("home/.claude/xshell-stats/broken.json", "not json");
        let s = get_claude_cost_summary(&fx.ctx());
        assert!(s.connected);
        let daily: Vec<(String, f64)> = s.daily.into_iter().map(|d| (d.date, d.usd)).collect();
        assert_eq!(
            daily,
            [
                ("2026-01-01".to_string(), 0.25),
                ("2026-01-02".to_string(), 3.75)
            ]
        );

        let s = get_claude_cost_summary(&Fixture::new().ctx());
        assert!(!s.connected);
        assert!(s.daily.is_empty());
    }
}
