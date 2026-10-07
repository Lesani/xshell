use crate::claude::get_claude_projects_dir;
use crate::paths::find_git_root;
use serde::Serialize;
use std::fs;
use std::io::{BufRead, BufReader};

#[derive(Serialize, Clone)]
pub struct Memory {
    name: String,
    description: String,
    #[serde(rename = "type")]
    kind: String,
    path: String,
}

#[derive(Serialize, Clone)]
pub struct ProjectMemories {
    dir: String,
    items: Vec<Memory>,
}

// Encode a filesystem path the same way Claude Code does when naming project dirs:
// replace any character that isn't alphanumeric / '_' / '-' with '-'.
pub fn encode_path_for_claude(path: &str) -> String {
    path.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

// Claude's auto-memory dir: ~/.claude/projects/<encoded-git-root>/memory/.
// All worktrees/subdirs within the same repo share one memory folder; outside a repo the
// project path itself is used. Each .md has YAML frontmatter (name/description/type);
// MEMORY.md is the index and is skipped.
pub fn get_project_memories(project_path: String) -> ProjectMemories {
    let projects_root = match get_claude_projects_dir() {
        Some(d) => d,
        None => {
            return ProjectMemories {
                dir: String::new(),
                items: vec![],
            }
        }
    };
    let pp = std::path::Path::new(&project_path);
    let repo_root = find_git_root(pp).unwrap_or_else(|| pp.to_path_buf());
    let encoded = encode_path_for_claude(&repo_root.to_string_lossy());
    let dir = projects_root.join(&encoded).join("memory");
    let dir_str = dir.to_string_lossy().to_string();
    if !dir.exists() {
        return ProjectMemories {
            dir: dir_str,
            items: vec![],
        };
    }

    let mut items: Vec<Memory> = Vec::new();
    for entry in fs::read_dir(&dir).ok().into_iter().flatten().flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "md") {
            continue;
        }
        let filename = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if filename.eq_ignore_ascii_case("MEMORY.md") {
            continue;
        }

        let mut name = filename.trim_end_matches(".md").to_string();
        let mut description = String::new();
        let mut kind = String::from("note");

        if let Ok(file) = fs::File::open(&path) {
            let mut in_fm = false;
            let mut fm_started = false;
            for line in BufReader::new(file).lines().take(30).flatten() {
                let trimmed = line.trim();
                if trimmed == "---" {
                    if !fm_started {
                        fm_started = true;
                        in_fm = true;
                        continue;
                    } else {
                        break;
                    }
                }
                if !in_fm {
                    continue;
                }
                if let Some(v) = trimmed.strip_prefix("name:") {
                    name = v.trim().trim_matches('"').to_string();
                } else if let Some(v) = trimmed.strip_prefix("description:") {
                    description = v.trim().trim_matches('"').to_string();
                } else if let Some(v) = trimmed.strip_prefix("type:") {
                    kind = v.trim().trim_matches('"').to_string();
                }
            }
        }
        items.push(Memory {
            name,
            description,
            kind,
            path: path.to_string_lossy().to_string(),
        });
    }
    items.sort_by(|a, b| {
        a.kind
            .cmp(&b.kind)
            .then(a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    ProjectMemories {
        dir: dir_str,
        items,
    }
}
