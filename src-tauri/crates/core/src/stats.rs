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

pub fn probe_statusline_setup() -> StatuslineProbe {
    let home = dirs::home_dir().unwrap_or_default();
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

pub fn get_global_rate_limits() -> GlobalRateLimits {
    let mut out = GlobalRateLimits {
        five_hour_pct: None,
        seven_day_pct: None,
        five_hour_resets_at: None,
        seven_day_resets_at: None,
        last_update_iso: None,
        source_session_id: None,
    };
    let Some(home) = dirs::home_dir() else {
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

pub fn get_claude_cost_summary() -> ClaudeCostSummary {
    let Some(home) = dirs::home_dir() else {
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
