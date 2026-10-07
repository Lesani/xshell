use crate::paths::paths_equal;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;

// ── Skills & Plugins ──────────────────────────────────────────────────
//
// Claude Code plugin storage layout (discovered on a real machine):
//   ~/.claude/plugins/installed_plugins.json  — truth for "what's installed"
//     { "plugins": { "<name>@<marketplace>": [ { scope: "user"|"local", projectPath?, installPath, version } ] } }
//   ~/.claude/settings.json                    — user-scope enabledPlugins map
//   <project>/.claude/settings.local.json      — local-scope enabledPlugins map (preferred)
//   <project>/.claude/settings.json            — ...fallback for local-scope
//   <installPath>/.claude-plugin/plugin.json   — plugin manifest (name, version, description)
//   <installPath>/skills/<name>/SKILL.md       — plugin-provided skills
//   <installPath>/.mcp.json                    — plugin-provided MCP servers
//   ~/.claude.json                             — user + per-project MCP servers
//     { mcpServers: {...}, projects: { "<path>": { mcpServers: {...} } } }

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Skill {
    pub name: String,
    pub scope: String, // "personal" | "project" | "plugin"
    pub description: Option<String>,
    pub path: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct McpInfo {
    pub name: String,
    pub kind: String,   // "http" | "stdio" | "sse" | "unknown"
    pub source: String, // "user" | "project" | "plugin"
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Plugin {
    pub name: String,
    pub marketplace: Option<String>,
    pub version: Option<String>,
    pub description: Option<String>,
    pub scope: String, // "user" | "local"
    pub enabled: bool,
    pub path: String,
    pub skills: Vec<Skill>,
    pub mcps: Vec<McpInfo>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SubagentInfo {
    pub name: String,
    pub path: String,
    pub scope: String, // "user" | "project"
    pub description: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SlashCommand {
    pub name: String,
    pub path: String,
    pub scope: String, // "user" | "project"
    pub description: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct HookEntry {
    pub event: String,
    pub matcher: Option<String>,
    pub command: String,
    pub source: String, // "user" | "project" | "local"
    pub source_path: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ClaudeMdFile {
    pub path: String,
    pub rel_path: String,
    pub scope: String, // "user" | "project-root" | "project-nested"
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SettingsSource {
    pub scope: String, // "user" | "project" | "local"
    pub path: String,
    pub exists: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ProjectSkills {
    pub personal_skills: Vec<Skill>,
    pub project_skills: Vec<Skill>,
    pub plugins: Vec<Plugin>,
    pub user_mcps: Vec<McpInfo>,
    pub project_mcps: Vec<McpInfo>,
    pub subagents: Vec<SubagentInfo>,
    pub slash_commands: Vec<SlashCommand>,
    pub hooks: Vec<HookEntry>,
    pub claude_md_files: Vec<ClaudeMdFile>,
    pub settings_sources: Vec<SettingsSource>,
}

pub fn parse_skill_description(md_path: &std::path::Path) -> Option<String> {
    let content = fs::read_to_string(md_path).ok()?;
    let trimmed = content.trim_start();
    if let Some(rest) = trimmed.strip_prefix("---") {
        if let Some(end) = rest.find("\n---") {
            let fm = &rest[..end];
            for line in fm.lines() {
                if let Some(v) = line.trim().strip_prefix("description:") {
                    let s = v.trim().trim_matches('"').trim_matches('\'');
                    if !s.is_empty() {
                        return Some(s.to_string());
                    }
                }
            }
        }
    }
    for line in content.lines() {
        if let Some(h) = line.trim().strip_prefix("# ") {
            return Some(h.trim().to_string());
        }
    }
    None
}

pub fn scan_skills_dir(dir: &std::path::Path, scope: &str) -> Vec<Skill> {
    let mut out = vec![];
    if !dir.exists() {
        return out;
    }
    for entry in fs::read_dir(dir).ok().into_iter().flatten().flatten() {
        if !entry.file_type().is_ok_and(|ft| ft.is_dir()) {
            continue;
        }
        let p = entry.path();
        let md = p.join("SKILL.md");
        if !md.exists() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        out.push(Skill {
            name,
            scope: scope.to_string(),
            description: parse_skill_description(&md),
            path: p.to_string_lossy().to_string(),
        });
    }
    out.sort_by_key(|a| a.name.to_lowercase());
    out
}

// Agents and slash commands are flat .md files (name = filename without extension). Uses
// the same frontmatter parser as skills — looks for `description:` then falls back to the
// first H1. Recurses into subdirectories so namespaced commands like `.claude/commands/git/commit.md`
// show up as "git/commit".
pub fn scan_md_entries(
    dir: &std::path::Path,
    scope: &str,
) -> Vec<(String, String, Option<String>)> {
    let mut out = vec![];
    if !dir.exists() {
        return out;
    }
    fn walk(cur: &std::path::Path, prefix: &str, out: &mut Vec<(String, String, Option<String>)>) {
        for entry in fs::read_dir(cur).ok().into_iter().flatten().flatten() {
            let p = entry.path();
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if ft.is_dir() {
                let name = entry.file_name().to_string_lossy().to_string();
                let new_prefix = if prefix.is_empty() {
                    name
                } else {
                    format!("{}/{}", prefix, name)
                };
                walk(&p, &new_prefix, out);
            } else if ft.is_file() && p.extension().map(|e| e == "md").unwrap_or(false) {
                let stem = p
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                let name = if prefix.is_empty() {
                    stem
                } else {
                    format!("{}/{}", prefix, stem)
                };
                let desc = parse_skill_description(&p);
                out.push((name, p.to_string_lossy().to_string(), desc));
            }
        }
    }
    walk(dir, "", &mut out);
    let _ = scope;
    out.sort_by_key(|a| a.0.to_lowercase());
    out
}

pub fn scan_subagents(dir: &std::path::Path, scope: &str) -> Vec<SubagentInfo> {
    scan_md_entries(dir, scope)
        .into_iter()
        .map(|(name, path, description)| SubagentInfo {
            name,
            path,
            scope: scope.to_string(),
            description,
        })
        .collect()
}

pub fn scan_slash_commands(dir: &std::path::Path, scope: &str) -> Vec<SlashCommand> {
    scan_md_entries(dir, scope)
        .into_iter()
        .map(|(name, path, description)| SlashCommand {
            name,
            path,
            scope: scope.to_string(),
            description,
        })
        .collect()
}

// Parses hooks from a settings.json file. Claude Code's format is:
//   { "hooks": { "PreToolUse": [ { "matcher": "Bash", "hooks": [ { "type": "command", "command": "..." } ] } ] } }
// Events without a matcher (Stop, UserPromptSubmit, etc.) just have the inner "hooks" array.
pub fn read_hooks_from(path: &std::path::Path, source: &str) -> Vec<HookEntry> {
    let mut out = vec![];
    let Ok(content) = fs::read_to_string(path) else {
        return out;
    };
    let Ok(json): Result<serde_json::Value, _> = serde_json::from_str(&content) else {
        return out;
    };
    let Some(hooks_obj) = json.get("hooks").and_then(|v| v.as_object()) else {
        return out;
    };
    let source_path = path.to_string_lossy().to_string();
    for (event, arr) in hooks_obj {
        let Some(arr) = arr.as_array() else {
            continue;
        };
        for entry in arr {
            let matcher = entry
                .get("matcher")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let Some(inner) = entry.get("hooks").and_then(|v| v.as_array()) else {
                continue;
            };
            for h in inner {
                let command = h
                    .get("command")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if command.is_empty() {
                    continue;
                }
                out.push(HookEntry {
                    event: event.clone(),
                    matcher: matcher.clone(),
                    command,
                    source: source.to_string(),
                    source_path: source_path.clone(),
                });
            }
        }
    }
    out
}

// Walk project looking for CLAUDE.md files. Depth-limited, skips common vendor/build dirs so
// a node_modules with a stray CLAUDE.md doesn't explode the tree.
pub fn scan_claude_md_files(
    project_path: &std::path::Path,
    home: &std::path::Path,
) -> Vec<ClaudeMdFile> {
    const SKIP: &[&str] = &[
        "node_modules",
        ".git",
        "dist",
        "build",
        "target",
        "out",
        ".next",
        ".venv",
        "venv",
        "__pycache__",
        ".claude",
        "coverage",
    ];
    const MAX_DEPTH: usize = 4;
    let mut out = vec![];

    // Project root (shown first, even if missing — no, only existing files).
    let root_md = project_path.join("CLAUDE.md");
    if root_md.exists() {
        out.push(ClaudeMdFile {
            path: root_md.to_string_lossy().to_string(),
            rel_path: "CLAUDE.md".to_string(),
            scope: "project-root".to_string(),
        });
    }

    // Nested — recurse, respecting depth and skip list.
    fn walk(
        base: &std::path::Path,
        cur: &std::path::Path,
        depth: usize,
        out: &mut Vec<ClaudeMdFile>,
    ) {
        if depth > MAX_DEPTH {
            return;
        }
        for entry in fs::read_dir(cur).ok().into_iter().flatten().flatten() {
            let p = entry.path();
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if ft.is_dir() {
                let name = entry.file_name().to_string_lossy().to_string();
                if SKIP.contains(&name.as_str()) || name.starts_with('.') {
                    continue;
                }
                walk(base, &p, depth + 1, out);
            } else if ft.is_file() && p.file_name().map(|n| n == "CLAUDE.md").unwrap_or(false) {
                // Skip the root one (already added).
                if p == base.join("CLAUDE.md") {
                    continue;
                }
                let rel = p
                    .strip_prefix(base)
                    .map(|r| r.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_else(|_| p.to_string_lossy().to_string());
                out.push(ClaudeMdFile {
                    path: p.to_string_lossy().to_string(),
                    rel_path: rel,
                    scope: "project-nested".to_string(),
                });
            }
        }
    }
    walk(project_path, project_path, 0, &mut out);

    // User-level (last, so project files lead).
    let user_md = home.join(".claude").join("CLAUDE.md");
    if user_md.exists() {
        out.push(ClaudeMdFile {
            path: user_md.to_string_lossy().to_string(),
            rel_path: "~/.claude/CLAUDE.md".to_string(),
            scope: "user".to_string(),
        });
    }
    out
}

pub fn scan_settings_sources(
    project_path: &std::path::Path,
    home: &std::path::Path,
) -> Vec<SettingsSource> {
    // Order: local first (wins), then project-shared, then user. UI renders them in the same
    // order so "the one that wins" is on top.
    let local = project_path.join(".claude").join("settings.local.json");
    let project = project_path.join(".claude").join("settings.json");
    let user = home.join(".claude").join("settings.json");
    vec![
        SettingsSource {
            scope: "local".to_string(),
            path: local.to_string_lossy().to_string(),
            exists: local.exists(),
        },
        SettingsSource {
            scope: "project".to_string(),
            path: project.to_string_lossy().to_string(),
            exists: project.exists(),
        },
        SettingsSource {
            scope: "user".to_string(),
            path: user.to_string_lossy().to_string(),
            exists: user.exists(),
        },
    ]
}

pub fn parse_plugin_manifest(
    manifest_path: &std::path::Path,
) -> Option<(String, Option<String>, Option<String>)> {
    let content = fs::read_to_string(manifest_path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;
    let name = json.get("name").and_then(|v| v.as_str())?.to_string();
    let version = json
        .get("version")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let description = json
        .get("description")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    Some((name, version, description))
}

// Read a JSON file's top-level `enabledPlugins: { "<key>": bool }` map.
pub fn read_enabled_plugins(path: &std::path::Path) -> HashMap<String, bool> {
    let mut out = HashMap::new();
    if let Ok(content) = fs::read_to_string(path) {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
            if let Some(obj) = json.get("enabledPlugins").and_then(|v| v.as_object()) {
                for (k, v) in obj {
                    if let Some(b) = v.as_bool() {
                        out.insert(k.clone(), b);
                    }
                }
            }
        }
    }
    out
}

pub fn parse_mcp_servers(
    obj: &serde_json::Map<String, serde_json::Value>,
    source: &str,
) -> Vec<McpInfo> {
    let mut out = vec![];
    for (name, entry) in obj {
        let kind = entry
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| {
                if entry.get("url").is_some() {
                    "http"
                } else if entry.get("command").is_some() {
                    "stdio"
                } else {
                    "unknown"
                }
            })
            .to_string();
        out.push(McpInfo {
            name: name.clone(),
            kind,
            source: source.to_string(),
        });
    }
    out.sort_by_key(|a| a.name.to_lowercase());
    out
}

pub fn read_plugin_mcps(install_path: &std::path::Path) -> Vec<McpInfo> {
    let mcp_json = install_path.join(".mcp.json");
    if !mcp_json.exists() {
        return vec![];
    }
    let Ok(content) = fs::read_to_string(&mcp_json) else {
        return vec![];
    };
    let Ok(json): Result<serde_json::Value, _> = serde_json::from_str(&content) else {
        return vec![];
    };
    match json.get("mcpServers").and_then(|v| v.as_object()) {
        Some(obj) => parse_mcp_servers(obj, "plugin"),
        None => vec![],
    }
}

pub fn parse_plugin_key(key: &str) -> (String, Option<String>) {
    match key.split_once('@') {
        Some((name, marketplace)) => (name.to_string(), Some(marketplace.to_string())),
        None => (key.to_string(), None),
    }
}

pub fn get_project_skills(project_path: String) -> ProjectSkills {
    let empty = || ProjectSkills {
        personal_skills: vec![],
        project_skills: vec![],
        plugins: vec![],
        user_mcps: vec![],
        project_mcps: vec![],
        subagents: vec![],
        slash_commands: vec![],
        hooks: vec![],
        claude_md_files: vec![],
        settings_sources: vec![],
    };
    let home = match dirs::home_dir() {
        Some(h) => h,
        None => return empty(),
    };

    // Regular skills (not plugin-bundled)
    let personal_skills = scan_skills_dir(&home.join(".claude").join("skills"), "personal");
    let project_skills = scan_skills_dir(
        &std::path::Path::new(&project_path)
            .join(".claude")
            .join("skills"),
        "project",
    );

    // Enabled maps
    let user_enabled = read_enabled_plugins(&home.join(".claude").join("settings.json"));
    let project_settings_local = std::path::Path::new(&project_path)
        .join(".claude")
        .join("settings.local.json");
    let project_settings = std::path::Path::new(&project_path)
        .join(".claude")
        .join("settings.json");
    let mut project_enabled = read_enabled_plugins(&project_settings_local);
    for (k, v) in read_enabled_plugins(&project_settings) {
        project_enabled.entry(k).or_insert(v);
    }

    // Installed plugins
    let mut plugins: Vec<Plugin> = vec![];
    let installed_path = home
        .join(".claude")
        .join("plugins")
        .join("installed_plugins.json");
    if let Ok(content) = fs::read_to_string(&installed_path) {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
            if let Some(obj) = json.get("plugins").and_then(|v| v.as_object()) {
                for (key, entries) in obj {
                    let (name, marketplace) = parse_plugin_key(key);
                    let Some(arr) = entries.as_array() else {
                        continue;
                    };
                    for entry in arr {
                        let scope = entry
                            .get("scope")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let entry_project = entry.get("projectPath").and_then(|v| v.as_str());
                        let install_path = entry
                            .get("installPath")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let version = entry
                            .get("version")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());

                        // Relevance filter: user-scope = global; local-scope = must match current project
                        let relevant = match scope.as_str() {
                            "user" => true,
                            "local" => entry_project
                                .map(|p| paths_equal(p, &project_path))
                                .unwrap_or(false),
                            _ => false,
                        };
                        if !relevant {
                            continue;
                        }

                        let enabled = match scope.as_str() {
                            "user" => user_enabled.get(key).copied().unwrap_or(false),
                            "local" => project_enabled.get(key).copied().unwrap_or(false),
                            _ => false,
                        };

                        let install_path_buf = std::path::PathBuf::from(&install_path);
                        let manifest = install_path_buf.join(".claude-plugin").join("plugin.json");
                        let (resolved_name, resolved_version, description) = if manifest.exists() {
                            let (n, v, d) = parse_plugin_manifest(&manifest)
                                .unwrap_or_else(|| (name.clone(), version.clone(), None));
                            (n, v.or(version.clone()), d)
                        } else {
                            (name.clone(), version.clone(), None)
                        };

                        let skills = scan_skills_dir(&install_path_buf.join("skills"), "plugin");
                        let mcps = read_plugin_mcps(&install_path_buf);

                        plugins.push(Plugin {
                            name: resolved_name,
                            marketplace: marketplace.clone(),
                            version: resolved_version,
                            description,
                            scope: scope.clone(),
                            enabled,
                            path: install_path,
                            skills,
                            mcps,
                        });
                    }
                }
            }
        }
    }
    // Show enabled first, then disabled; alpha within each group.
    plugins.sort_by(|a, b| {
        b.enabled
            .cmp(&a.enabled)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    // Standalone MCPs from ~/.claude.json (user-level + per-project)
    let mut user_mcps: Vec<McpInfo> = vec![];
    let mut project_mcps: Vec<McpInfo> = vec![];
    let claude_json_path = home.join(".claude.json");
    if let Ok(content) = fs::read_to_string(&claude_json_path) {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
            if let Some(obj) = json.get("mcpServers").and_then(|v| v.as_object()) {
                user_mcps = parse_mcp_servers(obj, "user");
            }
            if let Some(projs) = json.get("projects").and_then(|v| v.as_object()) {
                for (k, v) in projs {
                    if paths_equal(k, &project_path) {
                        if let Some(obj) = v.get("mcpServers").and_then(|v| v.as_object()) {
                            project_mcps = parse_mcp_servers(obj, "project");
                        }
                    }
                }
            }
        }
    }

    // ── Subagents (.claude/agents/*.md) ───────────────────────────────
    let mut subagents = vec![];
    subagents.extend(scan_subagents(
        &std::path::Path::new(&project_path)
            .join(".claude")
            .join("agents"),
        "project",
    ));
    subagents.extend(scan_subagents(&home.join(".claude").join("agents"), "user"));

    // ── Slash Commands (.claude/commands/*.md) ────────────────────────
    let mut slash_commands = vec![];
    slash_commands.extend(scan_slash_commands(
        &std::path::Path::new(&project_path)
            .join(".claude")
            .join("commands"),
        "project",
    ));
    slash_commands.extend(scan_slash_commands(
        &home.join(".claude").join("commands"),
        "user",
    ));

    // ── Hooks (merged from all three settings files) ──────────────────
    let mut hooks = vec![];
    hooks.extend(read_hooks_from(
        &std::path::Path::new(&project_path)
            .join(".claude")
            .join("settings.local.json"),
        "local",
    ));
    hooks.extend(read_hooks_from(
        &std::path::Path::new(&project_path)
            .join(".claude")
            .join("settings.json"),
        "project",
    ));
    hooks.extend(read_hooks_from(
        &home.join(".claude").join("settings.json"),
        "user",
    ));

    // ── CLAUDE.md files ───────────────────────────────────────────────
    let claude_md_files = scan_claude_md_files(std::path::Path::new(&project_path), &home);

    // ── Settings sources (merged view, local > project > user) ────────
    let settings_sources = scan_settings_sources(std::path::Path::new(&project_path), &home);

    ProjectSkills {
        personal_skills,
        project_skills,
        plugins,
        user_mcps,
        project_mcps,
        subagents,
        slash_commands,
        hooks,
        claude_md_files,
        settings_sources,
    }
}
