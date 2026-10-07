use crate::ctx::HostCtx;
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
pub fn get_project_memories(ctx: &HostCtx, project_path: String) -> ProjectMemories {
    let projects_root = match ctx.claude_projects_dir() {
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

#[cfg(test)]
mod tests {
    use super::*;

    use crate::testutil::Fixture;
    use serde_json::json;

    #[test]
    fn get_project_memories_uses_git_root_and_skips_index() {
        let fx = Fixture::new();
        // The repo root holds `.git`; the project is a subdirectory of it, so memories are
        // looked up under the encoded repo root, not the encoded project path.
        let repo = fx.dir.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let project = repo.join("pkg").join("app");
        std::fs::create_dir_all(&project).unwrap();
        let encoded = encode_path_for_claude(&repo.to_string_lossy());
        let mem = format!("home/.claude/projects/{encoded}/memory");
        fx.write(format!("{mem}/MEMORY.md"), "- index\n");
        fx.write(
            format!("{mem}/zeta.md"),
            "---\nname: \"Zeta rule\"\ndescription: Always zeta\ntype: feedback\n---\nbody\n",
        );
        fx.write(
            format!("{mem}/alpha.md"),
            "---\nname: alpha fact\ndescription: \"A\"\ntype: project\n---\n",
        );
        fx.write(format!("{mem}/plain.md"), "no frontmatter\n");
        fx.write(format!("{mem}/ignored.txt"), "not markdown");

        let r = serde_json::to_value(get_project_memories(
            &fx.ctx(),
            project.to_string_lossy().to_string(),
        ))
        .unwrap();
        let dir = fx
            .home()
            .join(".claude")
            .join("projects")
            .join(&encoded)
            .join("memory");
        assert_eq!(r["dir"], json!(dir.to_string_lossy()));
        let items: Vec<(String, String, String)> = r["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| {
                (
                    m["type"].as_str().unwrap().to_string(),
                    m["name"].as_str().unwrap().to_string(),
                    m["description"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        // Sorted by type, then case-insensitive name; MEMORY.md and non-.md files skipped.
        assert_eq!(
            items,
            [
                ("feedback".into(), "Zeta rule".into(), "Always zeta".into()),
                ("note".into(), "plain".into(), String::new()),
                ("project".into(), "alpha fact".into(), "A".into()),
            ]
        );
    }

    #[test]
    fn get_project_memories_without_home_is_empty() {
        let fx = Fixture::new();
        let ctx = HostCtx {
            home: None,
            ..fx.ctx()
        };
        let r = serde_json::to_value(get_project_memories(&ctx, "/p".into())).unwrap();
        assert_eq!(r, json!({"dir": "", "items": []}));
    }
}
