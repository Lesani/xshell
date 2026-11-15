//! Sending this Desktop's build to a Daemon over its link (`daemon.stage-*`, capability
//! `daemon.stage`; ADR-0019): no SSH, so it works for a Ring Host reached through the Relay.
//! The Daemon checks everything again; this side only picks the right binary and keeps the
//! pieces in order.

use crate::cancel::CancelToken;
use crate::errors::HostError;
use crate::install::{check_fetched, BinarySource, Wanted};
use crate::link::Link;
use base64::Engine;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;
use xshell_protocol::build::{install_dir_name, BuildId};
use xshell_protocol::msg::{
    ClientMsg, StageBegun, StageCommitted, StageReceived, UpdateInfo, STAGE_MAX_BYTES,
};

/// Bytes per `daemon.stage-chunk`.
pub const CHUNK: usize = 256 * 1024;
/// Chunks sent before their answers come back.
pub const IN_FLIGHT: usize = 4;
/// One answer may take this long (a slow disk, the Relay).
const STEP_TIMEOUT: Duration = Duration::from_secs(60);
/// Commit runs the build once (`--version`, at most 10 s on the Daemon).
const COMMIT_TIMEOUT: Duration = Duration::from_secs(90);

/// Send `msg` and wait for its answer.
fn ask(link: &Link, msg: ClientMsg, timeout: Duration) -> Result<Value, HostError> {
    let (tx, rx) = mpsc::channel();
    link.request(
        msg,
        timeout,
        Box::new(move |r| {
            let _ = tx.send(r);
        }),
    )?;
    rx.recv_timeout(timeout + Duration::from_secs(5))
        .unwrap_or_else(|_| Err(HostError::offline("the Host did not answer in time")))
}

fn parse<T: serde::de::DeserializeOwned>(v: Value) -> Result<T, HostError> {
    serde_json::from_value(v)
        .map_err(|e| HostError::offline(format!("unexpected answer from the Host: {e}")))
}

/// `daemon.update-info`.
pub fn update_info(link: &Link) -> Result<UpdateInfo, HostError> {
    parse(ask(link, ClientMsg::DaemonUpdateInfo, STEP_TIMEOUT)?)
}

/// Why sending a build did not work, for the status line.
#[derive(Debug)]
pub enum StageError {
    /// This Desktop has no build for the Host's platform.
    NoBuild {
        target: String,
        detail: String,
    },
    Failed(HostError),
}

impl From<HostError> for StageError {
    fn from(e: HostError) -> Self {
        StageError::Failed(e)
    }
}

/// Send `version`+`build` to the Daemon behind `link` and install it there (not selected).
/// `between` runs before every step and stops the send when it fails (a cancelled update,
/// a changed link). Returns the install directory.
pub fn stage_over_link(
    link: &Arc<Link>,
    binaries: &dyn BinarySource,
    version: &str,
    build: &BuildId,
    cancel: &CancelToken,
    between: &dyn Fn() -> Result<(), HostError>,
) -> Result<String, StageError> {
    let info = update_info(link)?;
    between()?;
    let want = Wanted {
        triple: info.target.clone(),
        version: version.to_string(),
        build: Some(build.clone()),
    };
    let fetched = binaries
        .fetch(&want, cancel)
        .and_then(|f| check_fetched(&f, &want).map(|()| f))
        .map_err(|detail| StageError::NoBuild {
            target: info.target.clone(),
            detail,
        })?;
    let bytes = fetched.bytes;
    let max = info.max_bytes.min(STAGE_MAX_BYTES);
    if bytes.is_empty() || bytes.len() as u64 > max {
        return Err(StageError::NoBuild {
            target: info.target,
            detail: format!(
                "the build is {} bytes; the Host takes at most {max}",
                bytes.len()
            ),
        });
    }
    between()?;
    let sha256 = format!("{:x}", Sha256::digest(&bytes));
    let begun: StageBegun = parse(ask(
        link,
        ClientMsg::DaemonStageBegin {
            version: version.to_string(),
            build: build.clone(),
            sha256,
            length: bytes.len() as u64,
        },
        STEP_TIMEOUT,
    )?)?;
    let dir = install_dir_name(version, Some(build));
    if begun.installed {
        return Ok(dir);
    }
    let stage = begun
        .stage
        .ok_or_else(|| HostError::offline("the Host started no update stage"))?;
    let sent = send_chunks(link, stage, &bytes, info.max_chunk, between).and_then(|()| {
        between()?;
        let c: StageCommitted = parse(ask(
            link,
            ClientMsg::DaemonStageCommit { stage },
            COMMIT_TIMEOUT,
        )?)?;
        Ok(c.dir)
    });
    match sent {
        Ok(d) if d == dir => Ok(d),
        Ok(d) => Err(HostError::offline(format!("the Host installed {d}, not {dir}")).into()),
        Err(e) => {
            // Best effort: the Daemon also drops it when the link closes or goes idle.
            let _ = link.request(
                ClientMsg::DaemonStageAbort { stage },
                STEP_TIMEOUT,
                Box::new(|_| {}),
            );
            Err(e.into())
        }
    }
}

/// The chunks in order, at most [`IN_FLIGHT`] unanswered; every answer must name the bytes
/// received so far.
fn send_chunks(
    link: &Arc<Link>,
    stage: Uuid,
    bytes: &[u8],
    max_chunk: u64,
    between: &dyn Fn() -> Result<(), HostError>,
) -> Result<(), HostError> {
    let size = CHUNK.min(max_chunk.max(1) as usize);
    let (tx, rx) = mpsc::channel::<(u64, Result<Value, HostError>)>();
    let mut waiting: VecDeque<u64> = VecDeque::new();
    let mut offset = 0usize;
    while offset < bytes.len() || !waiting.is_empty() {
        while offset < bytes.len() && waiting.len() < IN_FLIGHT {
            between()?;
            let end = (offset + size).min(bytes.len());
            let data = base64::engine::general_purpose::STANDARD.encode(&bytes[offset..end]);
            let tx = tx.clone();
            let expect = end as u64;
            link.request(
                ClientMsg::DaemonStageChunk {
                    stage,
                    offset: offset as u64,
                    data,
                },
                STEP_TIMEOUT,
                Box::new(move |r| {
                    let _ = tx.send((expect, r));
                }),
            )?;
            waiting.push_back(expect);
            offset = end;
        }
        let (got_for, r) = rx
            .recv_timeout(STEP_TIMEOUT + Duration::from_secs(5))
            .map_err(|_| HostError::offline("the Host did not answer in time"))?;
        let received: StageReceived = parse(r?)?;
        let expect = waiting
            .pop_front()
            .ok_or_else(|| HostError::offline("an unexpected answer from the Host"))?;
        if got_for != expect || received.received != expect {
            return Err(HostError::offline(format!(
                "the Host received {} bytes, expected {expect}",
                received.received
            )));
        }
    }
    Ok(())
}
