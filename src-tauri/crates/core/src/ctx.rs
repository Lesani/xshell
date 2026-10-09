use std::path::{Path, PathBuf};

/// Where a host's user files live. Every core function that reads under the user's home
/// directory or writes to the temp directory takes this instead of asking the process, so
/// tests can point it at a fixture tree and a daemon can serve the user it runs as.
///
/// Deliberately still read from the process environment, because they are not user-file
/// reads and a host daemon runs as the same user anyway: `get_username` (`USERNAME`/`USER`),
/// the Git Bash lookup (`ProgramFiles*`, `LOCALAPPDATA`) and `PATH` lookups for agent CLIs.
#[derive(Debug, Clone)]
pub struct HostCtx {
    /// `None` mirrors `dirs::home_dir()` returning `None`: every home-based lookup then
    /// behaves exactly as it did when it asked `dirs` directly.
    pub home: Option<PathBuf>,
    /// `std::env::temp_dir()` in production.
    pub temp_dir: PathBuf,
}

impl HostCtx {
    /// The current process's home and temp directories.
    pub fn from_env() -> Self {
        Self {
            home: dirs::home_dir(),
            temp_dir: std::env::temp_dir(),
        }
    }

    pub fn with_home(home: impl Into<PathBuf>, temp_dir: impl Into<PathBuf>) -> Self {
        Self {
            home: Some(home.into()),
            temp_dir: temp_dir.into(),
        }
    }

    pub fn home(&self) -> Option<&Path> {
        self.home.as_deref()
    }

    /// `~/.claude/projects`, where Claude Code keeps one directory per project.
    pub(crate) fn claude_projects_dir(&self) -> Option<PathBuf> {
        self.home
            .as_ref()
            .map(|h| h.join(".claude").join("projects"))
    }
}
