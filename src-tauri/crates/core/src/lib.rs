//! Tauri-free core of xshell: everything the desktop app and a remote host daemon share.

pub mod agent_context;
pub mod agent_status;
pub mod agents;
pub mod antigravity;
pub mod claude;
pub mod codex;
pub mod ctx;
pub mod cursor;
pub mod dispatch;
pub mod files;
pub mod git;
pub mod launch;
pub mod memories;
pub mod opencode;
pub mod paths;
pub mod sessions;
pub mod skills;
pub mod stats;
pub mod terminal;
pub mod time;

#[cfg(test)]
pub(crate) mod testutil;

pub use ctx::HostCtx;
pub use dispatch::{dispatch, METHODS};
pub use launch::{plan_command, plan_command_with, CommandPlan, LaunchSpec};
