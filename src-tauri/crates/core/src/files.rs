use serde::{Deserialize, Serialize};
use std::fs;

pub fn read_text_file(path: String) -> Result<String, String> {
    fs::read_to_string(&path).map_err(|e| format!("Failed to read file: {}", e))
}

pub fn decode_base64(s: &str) -> Vec<u8> {
    let mut val = 0u32;
    let mut bits = 0u32;
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    for c in s.chars() {
        let sextet = match c {
            'A'..='Z' => c as u32 - 'A' as u32,
            'a'..='z' => c as u32 - 'a' as u32 + 26,
            '0'..='9' => c as u32 - '0' as u32 + 52,
            '+' => 62,
            '/' => 63,
            '=' => break,
            _ => continue, // ignore whitespace/newlines
        };
        val = (val << 6) | sextet;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((val >> bits) & 0xFF) as u8);
        }
    }
    out
}

pub const MAX_DROPPED_FILE_BYTES: usize = 25 * 1024 * 1024; // ponytail: flat cap, revisit if legit >25MB drops show up

// Ctrl+V with an image on the clipboard, or a file dragged in from Explorer — WebView2 (unlike
// a native app) never gives us a real filesystem path for either one, so we save the bytes to
// a real temp file and hand back that path instead, since shells/CLIs take a path, not raw bytes.
// Bytes travel as base64 (not a JSON number array) to keep the IPC payload small.
pub fn save_dropped_file(bytes_base64: String, name: String) -> Result<String, String> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let bytes = decode_base64(&bytes_base64);
    if bytes.len() > MAX_DROPPED_FILE_BYTES {
        return Err(format!(
            "File too large ({} bytes, max {})",
            bytes.len(),
            MAX_DROPPED_FILE_BYTES
        ));
    }
    let dir = std::env::temp_dir().join("xshell-clipboard");
    fs::create_dir_all(&dir).map_err(|e| format!("Failed to create temp dir: {}", e))?;
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_millis();
    let safe_name: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ' '))
        .collect();
    let safe_name = if safe_name.is_empty() {
        "file".to_string()
    } else {
        safe_name
    };
    let path = dir.join(format!("{}-{}", ts, safe_name));
    fs::write(&path, &bytes).map_err(|e| format!("Failed to write file: {}", e))?;
    Ok(path.to_string_lossy().to_string())
}

// Called once at startup — the dir only ever grows otherwise (screenshots, dropped images...).
pub fn cleanup_old_dropped_files() {
    use std::time::{Duration, SystemTime};
    let dir = std::env::temp_dir().join("xshell-clipboard");
    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };
    let cutoff = Duration::from_secs(3 * 24 * 60 * 60);
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        if SystemTime::now()
            .duration_since(modified)
            .unwrap_or_default()
            > cutoff
        {
            let _ = fs::remove_file(entry.path());
        }
    }
}

pub fn get_username() -> String {
    std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "user".to_string())
}

pub fn get_home_dir() -> String {
    dirs::home_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

// ── File explorer ─────────────────────────────────────────────────────
//
// Single-level directory listing for the terminal's file-explorer panel. Lazy by design:
// the frontend calls this once per folder as the user expands it (mirroring the git panel's
// lazy-polling philosophy) rather than walking the whole tree up front. Returns folders
// first then files, each case-insensitively alphabetical — the order VS Code's explorer uses.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct DirItem {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
}

pub fn list_dir(path: String) -> Result<Vec<DirItem>, String> {
    let rd = fs::read_dir(&path).map_err(|e| format!("Failed to read {}: {}", path, e))?;
    let mut items: Vec<DirItem> = Vec::new();
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // file_type() avoids a stat syscall on most platforms; for symlinks we follow to
        // decide tree-vs-leaf so a symlinked dir still gets an expand chevron.
        let is_dir = match entry.file_type() {
            Ok(ft) if ft.is_symlink() => entry.path().is_dir(),
            Ok(ft) => ft.is_dir(),
            Err(_) => false,
        };
        items.push(DirItem {
            name,
            path: entry.path().to_string_lossy().into_owned(),
            is_dir,
        });
    }
    items.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(items)
}

// Recursive name search under `root` for the file-explorer's search box. Case-insensitive
// substring match on the entry name. Bounded on both axes — at most `limit` matches and a
// hard ceiling on entries visited — so searching a deep tree (or one with node_modules)
// stays responsive rather than walking millions of paths. Symlinked dirs aren't followed,
// which both avoids cycles and keeps the walk bounded.
//
// `async` so Tauri runs it on the async runtime rather than the main thread — a big tree can
// take a moment to walk, and doing it on the main thread would freeze the UI until it returns.
pub fn search_dir(root: String, query: String, limit: Option<usize>) -> Vec<DirItem> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return vec![];
    }
    let cap = limit.unwrap_or(300).min(2000);
    const MAX_VISIT: usize = 200_000;
    let mut out: Vec<DirItem> = Vec::new();
    let mut stack: Vec<std::path::PathBuf> = vec![std::path::PathBuf::from(&root)];
    let mut visited = 0usize;
    while let Some(dir) = stack.pop() {
        if out.len() >= cap || visited >= MAX_VISIT {
            break;
        }
        let rd = match fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        for entry in rd.flatten() {
            visited += 1;
            if visited >= MAX_VISIT {
                break;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            // Don't follow symlinks: treat them as leaves so the walk can't loop or escape.
            let is_dir = matches!(entry.file_type(), Ok(ft) if ft.is_dir() && !ft.is_symlink());
            if out.len() < cap && name.to_lowercase().contains(&q) {
                out.push(DirItem {
                    name,
                    path: entry.path().to_string_lossy().into_owned(),
                    is_dir,
                });
            }
            if is_dir {
                stack.push(entry.path());
            }
        }
    }
    out.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_base64_handles_padding_and_whitespace() {
        assert_eq!(decode_base64("aGVs\nbG8="), b"hello");
    }
}
