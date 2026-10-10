//! Tauri-free core of xshell: everything the desktop app and a remote host daemon share.

pub mod agent_context;
pub mod agent_status;
pub mod agents;
pub mod antigravity;
pub mod chat;
pub mod claude;
pub mod codex;
pub mod ctx;
pub mod cursor;
pub mod dispatch;
pub mod files;
pub mod git;
#[cfg(windows)]
pub mod job;
pub mod last_line;
pub mod launch;
pub mod memories;
pub mod opencode;
pub mod past;
pub mod paths;
pub mod pipe;
pub mod private_fs;
pub mod sessions;
pub mod skills;
pub mod stats;
pub mod terminal;
pub mod time;

#[cfg(test)]
pub(crate) mod testutil;

pub use ctx::HostCtx;
pub use dispatch::{dispatch, METHODS};
pub use launch::{
    first_message_args, plan_command, plan_command_first, plan_command_with, CommandPlan,
    LaunchSpec, FIRST_MESSAGE_MAX_BYTES,
};
