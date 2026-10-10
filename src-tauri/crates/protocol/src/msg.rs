//! Kind-0 JSON messages. Every message is an object with a string `"t"` (its type); fields are
//! camelCase and unknown fields are ignored, so protocol changes stay additive.

use crate::frame::{encode_json, FrameError};
use crate::launch::LaunchSpec;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fmt;
use uuid::Uuid;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub struct ProtocolRange {
    pub min: u32,
    pub max: u32,
}

impl fmt::Display for ProtocolRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}..{}", self.min, self.max)
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Hello {
    pub protocol: ProtocolRange,
    pub version: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

/// A Desktop → Host message. `id` lives on the envelope: when present the Host answers with
/// `res`; when absent (the Desktop's choice for high-rate `term.input`/`term.resize`) it does
/// not.
#[derive(Clone, PartialEq, Debug)]
pub struct Inbound {
    pub id: Option<u64>,
    pub msg: ClientMsg,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(tag = "t")]
pub enum ClientMsg {
    #[serde(rename = "hello")]
    Hello(Hello),
    #[serde(rename = "call")]
    Call {
        method: String,
        #[serde(default)]
        params: Value,
    },
    #[serde(rename = "term.open")]
    TermOpen { spec: OpenSpec },
    #[serde(rename = "term.attach")]
    TermAttach { terminal: Uuid },
    #[serde(rename = "term.detach")]
    TermDetach { terminal: Uuid },
    #[serde(rename = "term.input")]
    TermInput { terminal: Uuid, data: String },
    #[serde(rename = "term.resize")]
    TermResize {
        terminal: Uuid,
        cols: u16,
        rows: u16,
    },
    #[serde(rename = "term.close")]
    TermClose { terminal: Uuid },
    #[serde(rename = "term.update", rename_all = "camelCase")]
    TermUpdate {
        terminal: Uuid,
        #[serde(default)]
        session_id: Option<String>,
        #[serde(default)]
        meta: Option<Map<String, Value>>,
    },
    /// End the Terminal's process and start it again under the same UUID with
    /// `skipPermissions` changed, resuming its session. Gated on the `term.relaunch`
    /// capability.
    #[serde(rename = "term.relaunch", rename_all = "camelCase")]
    TermRelaunch {
        terminal: Uuid,
        skip_permissions: bool,
    },
    #[serde(rename = "daemon.upgrade")]
    DaemonUpgrade,
    /// An agent hook reporting the Agent Status of the Terminal it runs in. Sent by the hook
    /// client on the Host itself (`xshelld event`, or the Desktop executable for Local Host
    /// Terminals), never by a Desktop's UI. `run` names the Terminal's process (a Relaunch
    /// or restore starts a new run): a report for an older run is refused. Gated on the
    /// `agent.status` capability.
    #[serde(rename = "term.event")]
    TermEvent {
        terminal: Uuid,
        #[serde(default)]
        run: u64,
        status: AgentStatus,
    },
    /// This Host's Ring identity: its public keys and name, and its membership if it has
    /// one. Creates the keys on first use; the private keys never leave the Host. Desktop
    /// only; gated on the `ring` capability.
    #[serde(rename = "ring.identity")]
    RingIdentity,
    /// Join (or follow) a Ring: the whole Roster chain from version 1, as tokens. The chain
    /// must verify and its head must list this Host as a `daemon`; a chain of the same Ring
    /// must extend the stored one. Desktop only; gated on the `ring` capability.
    ///
    /// With `expect` (gated on `ring.cjoin`; an older Daemon ignores the field) the join is
    /// conditional: refused with [`MEMBERSHIP_CHANGED`] unless this Host's membership, as
    /// `ring.identity` reports it, is still the expected one.
    #[serde(rename = "ring.join")]
    RingJoin {
        rosters: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expect: Option<JoinExpect>,
    },
    /// A Mobile's push registration (over its session): the Push Gateway's opaque `blob`
    /// (`xpb1.…`), the X25519 `sealKey` pushes are sealed to (b64u; never the Mobile's
    /// session key), and which Agent Status changes push. Replaces the Mobile's previous
    /// registration. Mobile only; gated on the `push` capability.
    #[serde(rename = "push.register", rename_all = "camelCase")]
    PushRegister {
        blob: String,
        seal_key: String,
        triggers: PushTriggers,
    },
    /// Forget this Mobile's push registration. Mobile only; gated on `push`.
    #[serde(rename = "push.unregister")]
    PushUnregister,
    /// Follow the conversation of agent Terminal `terminal` (a direct Claude or Codex): the
    /// `res` is its newest [`SessionPage`] of at most `limit` entries ([`CHAT_PAGE_DEFAULT`],
    /// at most [`CHAT_PAGE_MAX`]), then `session.append` messages carry what is added. Replaces
    /// this connection's earlier subscription to the same Terminal. Gated on the
    /// `session.stream` capability.
    #[serde(rename = "session.subscribe")]
    SessionSubscribe {
        terminal: Uuid,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u32>,
    },
    /// The entries before `before` (a cursor from a page or a reset) of generation `gen` of
    /// this connection's subscription to `terminal`: an older [`SessionPage`]. Refused with
    /// [`SESSION_CHANGED`] once the subscription has moved to another generation.
    #[serde(rename = "session.page")]
    SessionPage {
        terminal: Uuid,
        gen: u64,
        before: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u32>,
    },
    /// End this connection's subscription to `terminal`; no `session.append` for it follows
    /// the `res`. Answers `null` also when there was none.
    #[serde(rename = "session.unsubscribe")]
    SessionUnsubscribe { terminal: Uuid },
}

/// The refusal of a `session.page` for a generation the subscription has left (a reset
/// follows or has already been sent).
pub const SESSION_CHANGED: &str = "session changed";
/// The refusal of a `session.page` without a subscription to that Terminal.
pub const NOT_SUBSCRIBED: &str = "not subscribed";
/// The refusal of a `session.subscribe` for a Terminal whose agent has no session stream
/// (anything but a direct Claude or Codex).
pub const NO_SESSION_STREAM: &str = "no session stream for this agent";

/// Entries a page or append carries by default and at most.
pub const CHAT_PAGE_DEFAULT: u32 = 50;
pub const CHAT_PAGE_MAX: u32 = 200;
/// The most bytes one page or append message takes, serialized as a frame: a `res` carrying
/// a [`SessionPage`], or a `session.append`. One entry alone always fits.
pub const CHAT_PAGE_MAX_BYTES: usize = 256 * 1024;
/// The most characters (Unicode scalar values) of a user or agent text.
pub const CHAT_TEXT_MAX_CHARS: usize = 32 * 1024;
/// The most characters of a tool call's input (compact JSON or raw text).
pub const CHAT_TOOL_INPUT_MAX_CHARS: usize = 2048;
/// The most characters of a tool result's text.
pub const CHAT_TOOL_RESULT_MAX_CHARS: usize = 4096;
/// The most characters of a tool's name and of a call id.
pub const CHAT_TOOL_NAME_MAX_CHARS: usize = 128;
/// The most characters of a tool call's one-line summary.
pub const CHAT_TOOL_SUMMARY_MAX_CHARS: usize = 200;

fn is_false(b: &bool) -> bool {
    !*b
}

/// One item of an agent conversation, as read from the agent's session. Text is cut at a
/// character boundary to its cap, with `truncated` set; nothing is ever HTML.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ChatItem {
    /// What the user typed (a slash command as `/name args`; an image as `[image]`).
    User {
        text: String,
        #[serde(default, skip_serializing_if = "is_false")]
        truncated: bool,
    },
    /// The agent's reply text (Markdown as the agent wrote it).
    Agent {
        text: String,
        #[serde(default, skip_serializing_if = "is_false")]
        truncated: bool,
    },
    /// The agent called a tool. `call` pairs it with its [`ChatItem::ToolResult`]; `summary`
    /// is one line (a command, a path, a pattern…); `input` the arguments, cut to
    /// [`CHAT_TOOL_INPUT_MAX_CHARS`] (`truncated`).
    ToolCall {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call: Option<String>,
        name: String,
        summary: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input: Option<String>,
        #[serde(default, skip_serializing_if = "is_false")]
        truncated: bool,
    },
    /// A tool's output, cut to [`CHAT_TOOL_RESULT_MAX_CHARS`]; `error` when the tool failed.
    ToolResult {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call: Option<String>,
        text: String,
        #[serde(default, skip_serializing_if = "is_false")]
        error: bool,
        #[serde(default, skip_serializing_if = "is_false")]
        truncated: bool,
    },
}

/// A [`ChatItem`] with its place in the conversation. `id` is unique within a subscription's
/// generation and stable across its pages and appends (`"<gen>:<offset>"`, or
/// `"<gen>:<offset>.<n>"` for the n-th item of one session line); `atMs` is when the agent
/// recorded it (Unix ms), if it did.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ChatEntry {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at_ms: Option<u64>,
    #[serde(flatten)]
    pub item: ChatItem,
}

/// Chat entries: one this side cannot read (an unknown `kind` from a newer Daemon, a missing
/// field, not an object) is dropped, never the whole message.
fn lenient_chat_entries<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Vec<ChatEntry>, D::Error> {
    let v = Option::<Vec<Value>>::deserialize(d)?;
    Ok(v.unwrap_or_default()
        .into_iter()
        .filter_map(|e| serde_json::from_value(e).ok())
        .collect())
}

/// A page of a conversation, oldest entry first: the answer to `session.subscribe` and
/// `session.page`.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct SessionPage {
    /// The subscription's generation; `session.page` names it.
    pub gen: u64,
    /// The agent session shown; `null` while the Terminal has none linked.
    pub session: Option<String>,
    #[serde(default, deserialize_with = "lenient_chat_entries")]
    pub items: Vec<ChatEntry>,
    /// An opaque cursor for the next older page (`session.page`'s `before`); absent at the
    /// start of the conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<u64>,
}

// ── Past sessions (`call get_project_sessions`, capability `project.sessions`) ──

/// The most sessions one `get_project_sessions` page holds; `limit` is clamped to
/// `1..=PAST_SESSIONS_PAGE_MAX` (default [`PAST_SESSIONS_PAGE_DEFAULT`]).
pub const PAST_SESSIONS_PAGE_MAX: u32 = 200;
pub const PAST_SESSIONS_PAGE_DEFAULT: u32 = 50;
/// A [`PastSession`]'s `title` is at most this many characters.
pub const PAST_SESSION_TITLE_MAX_CHARS: usize = 200;
/// A [`PastSession`]'s `gitBranch` is at most this many characters.
pub const PAST_SESSION_BRANCH_MAX_CHARS: usize = 100;

/// Where the next `get_project_sessions` page starts: the last session of the page before.
/// Pages are ordered by `modifiedMs` (newest first), then `agent`, then `id`.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PastSessionsCursor {
    pub modified_ms: u64,
    pub agent: String,
    pub id: String,
}

/// One past Claude Code or Codex session of a Project, as a Mobile lists it.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PastSession {
    pub id: String,
    /// `claude` or `codex`.
    pub agent: String,
    /// The session's name or first prompt, whitespace collapsed; empty when it has none.
    pub title: String,
    /// When the session's file last changed (Unix ms): the order and the time shown.
    pub modified_ms: u64,
    /// The prompts the user typed. A lower bound for a very large session, whose middle is
    /// not read.
    pub message_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
}

/// The answer to `get_project_sessions`.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PastSessionsPage {
    pub sessions: Vec<PastSession>,
    /// The `before` of the next page; `null` on the last page.
    pub next: Option<PastSessionsCursor>,
}

impl PastSession {
    /// This session as the `before` of the page after it.
    pub fn cursor(&self) -> PastSessionsCursor {
        PastSessionsCursor {
            modified_ms: self.modified_ms,
            agent: self.agent.clone(),
            id: self.id.clone(),
        }
    }
}

/// Which Agent Status changes wake a Mobile. Both are required.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PushTriggers {
    pub needs_you: bool,
    pub finished: bool,
}

/// The refusal of a conditional `ring.join` whose expected membership no longer holds.
pub const MEMBERSHIP_CHANGED: &str = "membership changed";

/// What a conditional `ring.join` expects this Host's membership to be.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct JoinExpect {
    /// The Ring the Host is in now; `null`: it is in none.
    pub ring_id: Option<String>,
    /// When set, that Ring's head version must be exactly this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u64>,
}

/// What an agent Terminal is doing, as its agent's hooks report it.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "kebab-case")]
pub enum AgentStatus {
    Working,
    /// A Permission Prompt or another question blocks the turn.
    NeedsYou,
    /// The turn ended.
    Finished,
    /// The agent exited. Final for the run.
    Ended,
}

impl AgentStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            AgentStatus::Working => "working",
            AgentStatus::NeedsYou => "needs-you",
            AgentStatus::Finished => "finished",
            AgentStatus::Ended => "ended",
        }
    }

    pub fn parse(s: &str) -> Option<AgentStatus> {
        serde_json::from_value(Value::String(s.to_string())).ok()
    }
}

/// `TerminalInfo.agent_status`: a value this side does not know (a newer Daemon's) reads as
/// absent, so one unknown status never fails the whole `terminals` list.
fn lenient_agent_status<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<AgentStatus>, D::Error> {
    let v = Option::<Value>::deserialize(d)?;
    Ok(v.and_then(|v| serde_json::from_value(v).ok()))
}

/// Who wrote a [`LastLine`].
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "kebab-case")]
pub enum Speaker {
    User,
    Agent,
}

/// The newest user or agent text message of an agent Terminal's session: one line,
/// whitespace collapsed, at most [`LAST_LINE_MAX_CHARS`] characters.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct LastLine {
    pub from: Speaker,
    pub text: String,
}

/// The most characters (Unicode scalar values) a [`LastLine`]'s text carries.
pub const LAST_LINE_MAX_CHARS: usize = 200;

/// `TerminalInfo.last_line`: a value this side cannot read (an unknown speaker, a missing
/// text, not an object) reads as absent, so it never fails the whole `terminals` list.
fn lenient_last_line<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<LastLine>, D::Error> {
    let v = Option::<Value>::deserialize(d)?;
    Ok(v.and_then(|v| serde_json::from_value(v).ok()))
}

/// Every `t` a Desktop may send in protocol 1.
const CLIENT_TYPES: &[&str] = &[
    "hello",
    "call",
    "term.open",
    "term.attach",
    "term.detach",
    "term.input",
    "term.resize",
    "term.close",
    "term.update",
    "term.relaunch",
    "daemon.upgrade",
    "term.event",
    "ring.identity",
    "ring.join",
    "push.register",
    "push.unregister",
    "session.subscribe",
    "session.page",
    "session.unsubscribe",
];

/// The longest `firstMessage` a `term.open` may carry, in bytes of UTF-8.
pub const FIRST_MESSAGE_MAX_BYTES: usize = 16 * 1024;

/// `term.open`'s spec: a launch spec plus the client-chosen UUID, initial size and opaque
/// display metadata (title, projectName, createdAt…).
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct OpenSpec {
    pub terminal: Uuid,
    #[serde(flatten)]
    pub launch: LaunchSpec,
    pub cols: u16,
    pub rows: u16,
    #[serde(default)]
    pub meta: Map<String, Value>,
    /// The prompt a new chat starts with (capability `term.first-message`). Open-time only:
    /// the Daemon passes it to this one launch and never persists, lists, restores or
    /// relaunches it. Only for a new direct Claude Code or Codex chat (no session to resume),
    /// at most [`FIRST_MESSAGE_MAX_BYTES`], not blank and without NUL; otherwise the open is
    /// refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_message: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct TerminalInfo {
    pub terminal: Uuid,
    pub spec: LaunchSpec,
    pub meta: Map<String, Value>,
    pub created_at_ms: u64,
    pub pid: Option<u32>,
    /// `Some` once the process ended; the Terminal stays listed until `term.close`.
    pub exit_code: Option<i32>,
    /// The Agent Status of an agent Terminal whose agent reports one. Absent for shells,
    /// agents without hooks, before the first report, and from Daemons without the
    /// `agent.status` capability.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient_agent_status"
    )]
    pub agent_status: Option<AgentStatus>,
    /// When the Agent Status last changed, in Unix milliseconds of the Daemon's clock.
    /// Strictly increasing per Terminal, across Relaunches and Daemon restarts while the
    /// Daemon remembers the previous value. Absent while `agent_status` is, and from Daemons
    /// that predate it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_at_ms: Option<u64>,
    /// The newest text message of the agent's session (capability `agent.last-line`).
    /// Absent for Terminals that are not a direct Claude or Codex agent, without a linked
    /// session, before the session has a text message, and from older Daemons.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient_last_line"
    )]
    pub last_line: Option<LastLine>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(tag = "t")]
pub enum ServerMsg {
    #[serde(rename = "hello")]
    Hello(Hello),
    #[serde(rename = "res")]
    Res(Res),
    #[serde(rename = "terminals")]
    Terminals { list: Vec<TerminalInfo> },
    #[serde(rename = "term.exit")]
    TermExit { terminal: Uuid, code: i32 },
    /// The size now applied to the Terminal's PTY. Sent only to a Mobile attached to the
    /// Terminal, whenever that size changes; never for the transient redraw nudge. Capability
    /// `term.mobile`.
    #[serde(rename = "term.size")]
    TermSize {
        terminal: Uuid,
        cols: u16,
        rows: u16,
    },
    /// Connection-level failure; the Host closes the connection after it.
    #[serde(rename = "error")]
    Error { code: String, message: String },
    /// New entries of a subscribed conversation (`session.subscribe`), in order, for
    /// generation `gen`. With `reset` the subscription moved to a new generation (another
    /// session was linked, the session file was replaced or truncated, or it appeared):
    /// `items` is its newest page and replaces everything shown, and `before` continues it.
    #[serde(rename = "session.append")]
    SessionAppend {
        terminal: Uuid,
        gen: u64,
        #[serde(default)]
        reset: bool,
        #[serde(default)]
        session: Option<String>,
        #[serde(default, deserialize_with = "lenient_chat_entries")]
        items: Vec<ChatEntry>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        before: Option<u64>,
    },
}

const SERVER_TYPES: &[&str] = &[
    "hello",
    "res",
    "terminals",
    "term.exit",
    "error",
    "session.append",
    "term.size",
];

/// `{"t":"res","id":7,"ok":<any>}` or `{"t":"res","id":7,"err":"…"}`.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Res {
    pub id: u64,
    #[serde(flatten)]
    pub outcome: Outcome,
}

/// Untagged rather than `Option<Value>`: an `Option` would read `"ok": null` back as `None`.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(untagged)]
pub enum Outcome {
    Ok { ok: Value },
    Err { err: String },
}

impl Outcome {
    pub fn from_result(r: Result<Value, String>) -> Self {
        match r {
            Ok(ok) => Outcome::Ok { ok },
            Err(err) => Outcome::Err { err },
        }
    }

    pub fn into_result(self) -> Result<Value, String> {
        match self {
            Outcome::Ok { ok } => Ok(ok),
            Outcome::Err { err } => Err(err),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum DecodeError {
    /// Not JSON, not an object, or no string `t`. The receiver closes the connection.
    Malformed(String),
    /// A well-formed message of a type this side does not know. Answered with an `err`.
    UnknownType { id: Option<u64>, t: String },
    /// A known type with bad fields. Answered with an `err`.
    Invalid {
        id: Option<u64>,
        t: String,
        error: String,
    },
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::Malformed(e) => write!(f, "malformed message: {e}"),
            DecodeError::UnknownType { t, .. } => write!(f, "unknown message type: {t}"),
            DecodeError::Invalid { t, error, .. } => write!(f, "invalid {t}: {error}"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Stage one: parse to a `Value` and read `t` and `id`, so a bad message of a known type can
/// still be answered under its id instead of closing the connection.
fn split(json: &[u8]) -> Result<(Value, String, Option<u64>), DecodeError> {
    let v: Value =
        serde_json::from_slice(json).map_err(|e| DecodeError::Malformed(e.to_string()))?;
    let obj = v
        .as_object()
        .ok_or_else(|| DecodeError::Malformed("not an object".into()))?;
    let t = obj
        .get("t")
        .and_then(Value::as_str)
        .ok_or_else(|| DecodeError::Malformed("missing string \"t\"".into()))?
        .to_string();
    let id = obj.get("id").and_then(Value::as_u64);
    Ok((v, t, id))
}

fn typed<T: serde::de::DeserializeOwned>(
    v: Value,
    t: String,
    id: Option<u64>,
    known: &[&str],
) -> Result<T, DecodeError> {
    if !known.contains(&t.as_str()) {
        return Err(DecodeError::UnknownType { id, t });
    }
    serde_json::from_value(v).map_err(|e| DecodeError::Invalid {
        id,
        t,
        error: e.to_string(),
    })
}

/// Decode a Desktop → Host message.
pub fn decode_inbound(json: &[u8]) -> Result<Inbound, DecodeError> {
    let (v, t, id) = split(json)?;
    let msg = typed(v, t, id, CLIENT_TYPES)?;
    Ok(Inbound { id, msg })
}

/// Decode a Host → Desktop message.
pub fn decode_server(json: &[u8]) -> Result<ServerMsg, DecodeError> {
    let (v, t, id) = split(json)?;
    typed(v, t, id, SERVER_TYPES)
}

/// Encode `msg` as one complete kind-0 frame, injecting `"id"` when given. Fails when the
/// frame would exceed `MAX_FRAME_LEN`.
pub fn encode_msg<T: Serialize>(msg: &T, id: Option<u64>) -> Result<Vec<u8>, FrameError> {
    let json = serde_json::to_vec(msg).expect("protocol messages always serialize");
    let json = match id {
        // Every message is a tagged object, so it starts with `{` and is never empty.
        Some(id) if json.first() == Some(&b'{') => {
            let mut v = format!("{{\"id\":{id},").into_bytes();
            v.extend_from_slice(&json[1..]);
            v
        }
        _ => json,
    };
    let mut out = Vec::with_capacity(json.len() + 5);
    encode_json(&json, &mut out)?;
    Ok(out)
}

/// Encode a `res`. A result too large for one frame becomes an `err` under the same id.
pub fn encode_res(id: u64, result: Result<Value, String>) -> Vec<u8> {
    let msg = ServerMsg::Res(Res {
        id,
        outcome: Outcome::from_result(result),
    });
    match encode_msg(&msg, None) {
        Ok(f) => f,
        Err(e) => {
            let err = match e {
                FrameError::TooLarge(n) => format!("response too large ({n} bytes)"),
                e => e.to_string(),
            };
            let msg = ServerMsg::Res(Res {
                id,
                outcome: Outcome::Err { err },
            });
            encode_msg(&msg, None).expect("a short err always fits")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{read_frame, Frame, MAX_FRAME_LEN};
    use serde_json::json;

    fn body(frame: Vec<u8>) -> Vec<u8> {
        match read_frame(&mut std::io::Cursor::new(frame), MAX_FRAME_LEN)
            .unwrap()
            .unwrap()
        {
            Frame::Json(b) => b,
            f => panic!("{f:?}"),
        }
    }

    #[test]
    fn hello_golden_json() {
        let h = ServerMsg::Hello(Hello {
            protocol: ProtocolRange { min: 1, max: 1 },
            version: "x".into(),
            capabilities: vec!["call".into()],
        });
        let b = body(encode_msg(&h, None).unwrap());
        assert_eq!(
            String::from_utf8(b).unwrap(),
            r#"{"t":"hello","protocol":{"min":1,"max":1},"version":"x","capabilities":["call"]}"#
        );
    }

    #[test]
    fn res_ok_null_roundtrip() {
        let b = body(encode_res(7, Ok(Value::Null)));
        assert_eq!(
            String::from_utf8(b.clone()).unwrap(),
            r#"{"t":"res","id":7,"ok":null}"#
        );
        assert_eq!(
            decode_server(&b).unwrap(),
            ServerMsg::Res(Res {
                id: 7,
                outcome: Outcome::Ok { ok: Value::Null }
            })
        );
    }

    #[test]
    fn res_err_roundtrip() {
        let b = body(encode_res(8, Err("boom".into())));
        let ServerMsg::Res(r) = decode_server(&b).unwrap() else {
            panic!()
        };
        assert_eq!(r.id, 8);
        assert_eq!(r.outcome.into_result(), Err("boom".to_string()));
    }

    #[test]
    fn res_too_large_becomes_err() {
        let big = "x".repeat(MAX_FRAME_LEN as usize);
        let b = body(encode_res(9, Ok(Value::String(big))));
        let ServerMsg::Res(r) = decode_server(&b).unwrap() else {
            panic!()
        };
        assert_eq!(r.id, 9);
        let err = r.outcome.into_result().unwrap_err();
        assert!(err.starts_with("response too large ("), "{err}");
    }

    #[test]
    fn open_spec_camel_case() {
        let id = Uuid::new_v4();
        let raw = json!({"t":"term.open","id":1,"spec":{"terminal":id,"sessionId":"s","cwd":"/x",
            "shellMode":"raw","cols":80,"rows":24,"meta":{"title":"T"}}});
        let inb = decode_inbound(raw.to_string().as_bytes()).unwrap();
        assert_eq!(inb.id, Some(1));
        let mut meta = Map::new();
        meta.insert("title".into(), json!("T"));
        assert_eq!(
            inb.msg,
            ClientMsg::TermOpen {
                spec: OpenSpec {
                    terminal: id,
                    launch: LaunchSpec {
                        session_id: Some("s".into()),
                        cwd: "/x".into(),
                        shell_mode: Some("raw".into()),
                        ..Default::default()
                    },
                    cols: 80,
                    rows: 24,
                    meta,
                    first_message: None,
                }
            }
        );
    }

    #[test]
    fn open_spec_first_message_golden() {
        let id = Uuid::new_v4();
        let spec = OpenSpec {
            terminal: id,
            launch: LaunchSpec {
                agent: Some("claude".into()),
                cwd: "/x".into(),
                ..Default::default()
            },
            cols: 120,
            rows: 40,
            first_message: Some("hello there".into()),
            ..Default::default()
        };
        let v = serde_json::to_value(&spec).unwrap();
        assert_eq!(v["firstMessage"], json!("hello there"));
        assert!(v.get("first_message").is_none());
        let back: OpenSpec = serde_json::from_value(v).unwrap();
        assert_eq!(back, spec);
        // Absent when `None`, and JSON from peers that predate it still decodes.
        let none = serde_json::to_value(OpenSpec {
            first_message: None,
            ..spec.clone()
        })
        .unwrap();
        assert!(none.get("firstMessage").is_none(), "{none}");
        let old = json!({"terminal":id,"agent":"claude","cwd":"/x","cols":120,"rows":40});
        let back: OpenSpec = serde_json::from_value(old).unwrap();
        assert_eq!(back.first_message, None);
        assert_eq!(back.launch.agent.as_deref(), Some("claude"));
    }

    #[test]
    fn term_update_camel_case() {
        let id = Uuid::new_v4();
        let raw = json!({"t":"term.update","terminal":id,"sessionId":"s2","meta":{"a":null}});
        let inb = decode_inbound(raw.to_string().as_bytes()).unwrap();
        let ClientMsg::TermUpdate {
            session_id, meta, ..
        } = inb.msg
        else {
            panic!()
        };
        assert_eq!(session_id.as_deref(), Some("s2"));
        assert_eq!(meta.unwrap().get("a"), Some(&Value::Null));
    }

    #[test]
    fn term_relaunch_camel_case() {
        let id = Uuid::new_v4();
        let msg = ClientMsg::TermRelaunch {
            terminal: id,
            skip_permissions: true,
        };
        let b = body(encode_msg(&msg, Some(4)).unwrap());
        assert_eq!(
            String::from_utf8(b.clone()).unwrap(),
            format!(r#"{{"id":4,"t":"term.relaunch","terminal":"{id}","skipPermissions":true}}"#)
        );
        assert_eq!(decode_inbound(&b).unwrap(), Inbound { id: Some(4), msg });
    }

    #[test]
    fn term_relaunch_missing_field_invalid_keeps_id() {
        let id = Uuid::new_v4();
        let raw = json!({"t":"term.relaunch","id":5,"terminal":id});
        let e = decode_inbound(raw.to_string().as_bytes()).unwrap_err();
        assert!(
            matches!(&e, DecodeError::Invalid { id: Some(5), t, .. } if t == "term.relaunch"),
            "{e:?}"
        );
    }

    #[test]
    fn unknown_type_keeps_id() {
        assert_eq!(
            decode_inbound(br#"{"t":"nope","id":5}"#).unwrap_err(),
            DecodeError::UnknownType {
                id: Some(5),
                t: "nope".into()
            }
        );
    }

    #[test]
    fn invalid_fields_keep_id() {
        let e = decode_inbound(br#"{"t":"term.resize","id":6,"terminal":"bad"}"#).unwrap_err();
        assert!(
            matches!(&e, DecodeError::Invalid { id: Some(6), t, .. } if t == "term.resize"),
            "{e:?}"
        );
        assert!(e.to_string().starts_with("invalid term.resize: "));
    }

    #[test]
    fn unknown_fields_ignored() {
        let id = Uuid::new_v4();
        let raw = json!({"t":"term.input","terminal":id,"data":"x","future":1});
        let inb = decode_inbound(raw.to_string().as_bytes()).unwrap();
        assert_eq!(
            inb.msg,
            ClientMsg::TermInput {
                terminal: id,
                data: "x".into()
            }
        );
    }

    #[test]
    fn malformed_json() {
        for bad in [&b"{"[..], b"[]", br#"{"x":1}"#, br#"{"t":5}"#] {
            assert!(
                matches!(decode_inbound(bad), Err(DecodeError::Malformed(_))),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    #[test]
    fn id_optional() {
        let id = Uuid::new_v4();
        let raw = json!({"t":"term.input","terminal":id,"data":"x"});
        assert_eq!(decode_inbound(raw.to_string().as_bytes()).unwrap().id, None);
    }

    #[test]
    fn encode_msg_injects_id() {
        let b = body(encode_msg(&ClientMsg::DaemonUpgrade, Some(3)).unwrap());
        assert_eq!(
            String::from_utf8(b.clone()).unwrap(),
            r#"{"id":3,"t":"daemon.upgrade"}"#
        );
        let inb = decode_inbound(&b).unwrap();
        assert_eq!(inb.id, Some(3));
        assert_eq!(inb.msg, ClientMsg::DaemonUpgrade);
    }

    #[test]
    fn ring_messages_roundtrip() {
        let b = body(encode_msg(&ClientMsg::RingIdentity, Some(4)).unwrap());
        assert_eq!(
            String::from_utf8(b.clone()).unwrap(),
            r#"{"id":4,"t":"ring.identity"}"#
        );
        assert_eq!(decode_inbound(&b).unwrap().msg, ClientMsg::RingIdentity);
        let join = ClientMsg::RingJoin {
            rosters: vec!["xro1.a.b".into()],
            expect: None,
        };
        let b = body(encode_msg(&join, Some(5)).unwrap());
        assert_eq!(
            String::from_utf8(b.clone()).unwrap(),
            r#"{"id":5,"t":"ring.join","rosters":["xro1.a.b"]}"#
        );
        assert_eq!(decode_inbound(&b).unwrap().msg, join);
        // Conditional: an unpaired Host, then one in a given Ring at a given version.
        let cjoin = ClientMsg::RingJoin {
            rosters: vec!["xro1.a.b".into()],
            expect: Some(JoinExpect {
                ring_id: None,
                version: None,
            }),
        };
        let b = body(encode_msg(&cjoin, Some(7)).unwrap());
        assert_eq!(
            String::from_utf8(b.clone()).unwrap(),
            r#"{"id":7,"t":"ring.join","rosters":["xro1.a.b"],"expect":{"ringId":null}}"#
        );
        assert_eq!(decode_inbound(&b).unwrap().msg, cjoin);
        let raw = json!({"t":"ring.join","id":8,"rosters":[],"expect":{"ringId":"r","version":3}});
        assert_eq!(
            decode_inbound(raw.to_string().as_bytes()).unwrap().msg,
            ClientMsg::RingJoin {
                rosters: vec![],
                expect: Some(JoinExpect {
                    ring_id: Some("r".into()),
                    version: Some(3)
                })
            }
        );
        let raw = json!({"t":"ring.join","id":6});
        assert!(matches!(
            decode_inbound(raw.to_string().as_bytes()),
            Err(DecodeError::Invalid { id: Some(6), .. })
        ));
    }

    #[test]
    fn push_messages_roundtrip() {
        let reg = ClientMsg::PushRegister {
            blob: "xpb1.k.AAAA".into(),
            seal_key: "S".into(),
            triggers: PushTriggers {
                needs_you: true,
                finished: false,
            },
        };
        let b = body(encode_msg(&reg, Some(7)).unwrap());
        assert_eq!(
            String::from_utf8(b.clone()).unwrap(),
            r#"{"id":7,"t":"push.register","blob":"xpb1.k.AAAA","sealKey":"S","triggers":{"needsYou":true,"finished":false}}"#
        );
        assert_eq!(decode_inbound(&b).unwrap().msg, reg);
        let b = body(encode_msg(&ClientMsg::PushUnregister, Some(8)).unwrap());
        assert_eq!(
            String::from_utf8(b.clone()).unwrap(),
            r#"{"id":8,"t":"push.unregister"}"#
        );
        assert_eq!(decode_inbound(&b).unwrap().msg, ClientMsg::PushUnregister);
        // Both triggers are required.
        let raw = json!({"t":"push.register","id":9,"blob":"b","sealKey":"k","triggers":{"needsYou":true}});
        assert!(matches!(
            decode_inbound(raw.to_string().as_bytes()),
            Err(DecodeError::Invalid { id: Some(9), .. })
        ));
    }

    #[test]
    fn terminals_roundtrip_skip_permissions() {
        let list = ServerMsg::Terminals {
            list: vec![TerminalInfo {
                terminal: Uuid::new_v4(),
                spec: LaunchSpec {
                    session_id: Some("s".into()),
                    skip_permissions: Some(true),
                    ..Default::default()
                },
                meta: Map::new(),
                created_at_ms: 5,
                pid: Some(9),
                exit_code: None,
                agent_status: None,
                status_at_ms: None,
                last_line: None,
            }],
        };
        let b = body(encode_msg(&list, None).unwrap());
        assert!(String::from_utf8_lossy(&b).contains("\"skipPermissions\":true"));
        assert_eq!(decode_server(&b).unwrap(), list);
    }

    #[test]
    fn terminals_and_exit_roundtrip() {
        let id = Uuid::new_v4();
        let list = ServerMsg::Terminals {
            list: vec![TerminalInfo {
                terminal: id,
                spec: LaunchSpec::default(),
                meta: Map::new(),
                created_at_ms: 5,
                pid: Some(9),
                exit_code: None,
                agent_status: None,
                status_at_ms: None,
                last_line: None,
            }],
        };
        let b = body(encode_msg(&list, None).unwrap());
        assert!(String::from_utf8_lossy(&b).contains("\"createdAtMs\":5"));
        assert_eq!(decode_server(&b).unwrap(), list);
        let exit = ServerMsg::TermExit {
            terminal: id,
            code: 7,
        };
        assert_eq!(
            decode_server(&body(encode_msg(&exit, None).unwrap())).unwrap(),
            exit
        );
    }

    #[test]
    fn term_size_golden_roundtrip() {
        let id = Uuid::parse_str("00000000-0000-0000-0000-000000000007").unwrap();
        let m = ServerMsg::TermSize {
            terminal: id,
            cols: 40,
            rows: 20,
        };
        let b = body(encode_msg(&m, None).unwrap());
        assert_eq!(
            String::from_utf8(b.clone()).unwrap(),
            r#"{"t":"term.size","terminal":"00000000-0000-0000-0000-000000000007","cols":40,"rows":20}"#
        );
        assert_eq!(decode_server(&b).unwrap(), m);
    }

    /// A decoder from before `term.size` reports it as an unknown type, which every client
    /// skips (additive protocol).
    #[test]
    fn term_size_unknown_to_older_decoder() {
        let b = body(
            encode_msg(
                &ServerMsg::TermSize {
                    terminal: Uuid::new_v4(),
                    cols: 40,
                    rows: 20,
                },
                None,
            )
            .unwrap(),
        );
        let (v, t, id) = split(&b).unwrap();
        let older: Vec<&str> = SERVER_TYPES
            .iter()
            .copied()
            .filter(|t| *t != "term.size")
            .collect();
        assert_eq!(
            typed::<ServerMsg>(v, t, id, &older).unwrap_err(),
            DecodeError::UnknownType {
                id: None,
                t: "term.size".into()
            }
        );
    }

    fn info_with(status: Option<AgentStatus>) -> TerminalInfo {
        TerminalInfo {
            terminal: Uuid::new_v4(),
            spec: LaunchSpec::default(),
            meta: Map::new(),
            created_at_ms: 5,
            pid: Some(9),
            exit_code: None,
            agent_status: status,
            status_at_ms: None,
            last_line: None,
        }
    }

    #[test]
    fn term_event_roundtrip() {
        let id = Uuid::new_v4();
        let msg = ClientMsg::TermEvent {
            terminal: id,
            run: 3,
            status: AgentStatus::NeedsYou,
        };
        let b = body(encode_msg(&msg, Some(1)).unwrap());
        assert_eq!(
            String::from_utf8(b.clone()).unwrap(),
            format!(
                r#"{{"id":1,"t":"term.event","terminal":"{id}","run":3,"status":"needs-you"}}"#
            )
        );
        assert_eq!(decode_inbound(&b).unwrap(), Inbound { id: Some(1), msg });
        // `run` is additive: a report without one is run 0.
        let raw = json!({"t":"term.event","terminal":id,"status":"working"});
        assert_eq!(
            decode_inbound(raw.to_string().as_bytes()).unwrap().msg,
            ClientMsg::TermEvent {
                terminal: id,
                run: 0,
                status: AgentStatus::Working
            }
        );
    }

    #[test]
    fn terminals_agent_status_kebab_case() {
        let list = ServerMsg::Terminals {
            list: vec![info_with(Some(AgentStatus::NeedsYou))],
        };
        let b = body(encode_msg(&list, None).unwrap());
        assert!(String::from_utf8_lossy(&b).contains(r#""agentStatus":"needs-you""#));
        assert_eq!(decode_server(&b).unwrap(), list);
        for (s, wire) in [
            (AgentStatus::Working, "working"),
            (AgentStatus::NeedsYou, "needs-you"),
            (AgentStatus::Finished, "finished"),
            (AgentStatus::Ended, "ended"),
        ] {
            assert_eq!(serde_json::to_value(s).unwrap(), json!(wire));
            assert_eq!(s.as_str(), wire);
            assert_eq!(AgentStatus::parse(wire), Some(s));
        }
        assert_eq!(AgentStatus::parse("bogus"), None);
    }

    #[test]
    fn terminals_agent_status_omitted_when_none() {
        let b = body(
            encode_msg(
                &ServerMsg::Terminals {
                    list: vec![info_with(None)],
                },
                None,
            )
            .unwrap(),
        );
        assert!(!String::from_utf8_lossy(&b).contains("agentStatus"));
    }

    #[test]
    fn terminal_info_without_agent_status_decodes() {
        // An older Daemon's entry.
        let id = Uuid::new_v4();
        let raw = json!({"t":"terminals","list":[{"terminal":id,"spec":{"cwd":"/w"},"meta":{},
            "createdAtMs":1,"pid":2,"exitCode":null}]});
        let ServerMsg::Terminals { list } = decode_server(raw.to_string().as_bytes()).unwrap()
        else {
            panic!()
        };
        assert_eq!(list[0].agent_status, None);
    }

    #[test]
    fn terminals_unknown_agent_status_decodes_as_none() {
        // A newer Daemon's status this side does not know never fails the list.
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let raw = json!({"t":"terminals","list":[
            {"terminal":a,"spec":{"cwd":"/w"},"meta":{},"createdAtMs":1,"pid":2,"exitCode":null,
             "agentStatus":"thinking-hard"},
            {"terminal":b,"spec":{"cwd":"/w"},"meta":{},"createdAtMs":1,"pid":3,"exitCode":null,
             "agentStatus":"finished"},
            {"terminal":b,"spec":{"cwd":"/w"},"meta":{},"createdAtMs":1,"pid":3,"exitCode":null,
             "agentStatus":7}]});
        let ServerMsg::Terminals { list } = decode_server(raw.to_string().as_bytes()).unwrap()
        else {
            panic!()
        };
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].agent_status, None);
        assert_eq!(list[1].agent_status, Some(AgentStatus::Finished));
        assert_eq!(list[2].agent_status, None);
    }

    #[test]
    fn term_event_unknown_status_invalid_keeps_id() {
        let id = Uuid::new_v4();
        let raw = json!({"t":"term.event","id":9,"terminal":id,"run":1,"status":"bogus"});
        let e = decode_inbound(raw.to_string().as_bytes()).unwrap_err();
        assert!(
            matches!(&e, DecodeError::Invalid { id: Some(9), t, .. } if t == "term.event"),
            "{e:?}"
        );
    }

    #[test]
    fn terminals_status_at_and_last_line_golden() {
        let id = Uuid::new_v4();
        let info = TerminalInfo {
            status_at_ms: Some(1_700_000_000_123),
            last_line: Some(LastLine {
                from: Speaker::Agent,
                text: "Refactor done.".into(),
            }),
            terminal: id,
            ..info_with(Some(AgentStatus::Finished))
        };
        let list = ServerMsg::Terminals { list: vec![info] };
        let b = body(encode_msg(&list, None).unwrap());
        let s = String::from_utf8(b.clone()).unwrap();
        assert!(
            s.ends_with(
                r#""agentStatus":"finished","statusAtMs":1700000000123,"lastLine":{"from":"agent","text":"Refactor done."}}]}"#
            ),
            "{s}"
        );
        assert_eq!(decode_server(&b).unwrap(), list);
        assert_eq!(serde_json::to_value(Speaker::User).unwrap(), json!("user"));
    }

    #[test]
    fn new_fields_omitted_when_none() {
        let b = body(
            encode_msg(
                &ServerMsg::Terminals {
                    list: vec![info_with(Some(AgentStatus::Working))],
                },
                None,
            )
            .unwrap(),
        );
        let s = String::from_utf8_lossy(&b);
        assert!(!s.contains("statusAtMs"), "{s}");
        assert!(!s.contains("lastLine"), "{s}");
    }

    #[test]
    fn terminal_info_from_older_daemon_decodes() {
        let id = Uuid::new_v4();
        let raw = json!({"t":"terminals","list":[{"terminal":id,"spec":{"cwd":"/w"},"meta":{},
            "createdAtMs":1,"pid":2,"exitCode":null,"agentStatus":"working"}]});
        let ServerMsg::Terminals { list } = decode_server(raw.to_string().as_bytes()).unwrap()
        else {
            panic!()
        };
        assert_eq!(list[0].agent_status, Some(AgentStatus::Working));
        assert_eq!(list[0].status_at_ms, None);
        assert_eq!(list[0].last_line, None);
    }

    #[test]
    fn bad_last_line_decodes_as_none() {
        let id = Uuid::new_v4();
        let entry = |last: Value| {
            json!({"terminal":id,"spec":{"cwd":"/w"},"meta":{},"createdAtMs":1,"pid":2,
                   "exitCode":null,"statusAtMs":5,"lastLine":last})
        };
        let raw = json!({"t":"terminals","list":[
            entry(json!({"from":"robot","text":"x"})),
            entry(json!(7)),
            entry(json!({"from":"user"})),
            entry(json!(null)),
            entry(json!({"from":"user","text":"hi"})),
        ]});
        let ServerMsg::Terminals { list } = decode_server(raw.to_string().as_bytes()).unwrap()
        else {
            panic!()
        };
        assert_eq!(list.len(), 5);
        for t in &list[..4] {
            assert_eq!(t.last_line, None);
            assert_eq!(t.status_at_ms, Some(5));
        }
        assert_eq!(
            list[4].last_line,
            Some(LastLine {
                from: Speaker::User,
                text: "hi".into()
            })
        );
    }

    #[test]
    fn session_subscribe_page_unsubscribe_golden() {
        let id = Uuid::new_v4();
        for (msg, want) in [
            (
                ClientMsg::SessionSubscribe {
                    terminal: id,
                    limit: None,
                },
                format!(r#"{{"id":3,"t":"session.subscribe","terminal":"{id}"}}"#),
            ),
            (
                ClientMsg::SessionSubscribe {
                    terminal: id,
                    limit: Some(20),
                },
                format!(r#"{{"id":3,"t":"session.subscribe","terminal":"{id}","limit":20}}"#),
            ),
            (
                ClientMsg::SessionPage {
                    terminal: id,
                    gen: 7,
                    before: 4096,
                    limit: Some(50),
                },
                format!(
                    r#"{{"id":3,"t":"session.page","terminal":"{id}","gen":7,"before":4096,"limit":50}}"#
                ),
            ),
            (
                ClientMsg::SessionUnsubscribe { terminal: id },
                format!(r#"{{"id":3,"t":"session.unsubscribe","terminal":"{id}"}}"#),
            ),
        ] {
            let b = body(encode_msg(&msg, Some(3)).unwrap());
            assert_eq!(String::from_utf8(b.clone()).unwrap(), want);
            assert_eq!(decode_inbound(&b).unwrap(), Inbound { id: Some(3), msg });
        }
        // `gen` and `before` are required on a page request.
        let raw = json!({"t":"session.page","id":4,"terminal":id,"before":1});
        assert!(matches!(
            decode_inbound(raw.to_string().as_bytes()),
            Err(DecodeError::Invalid { id: Some(4), .. })
        ));
    }

    fn sample_entries() -> Vec<ChatEntry> {
        vec![
            ChatEntry {
                id: "2:0".into(),
                at_ms: Some(1_700_000_000_000),
                item: ChatItem::User {
                    text: "fix it".into(),
                    truncated: false,
                },
            },
            ChatEntry {
                id: "2:90.1".into(),
                at_ms: None,
                item: ChatItem::ToolCall {
                    call: Some("toolu_1".into()),
                    name: "Bash".into(),
                    summary: "npm test".into(),
                    input: Some(r#"{"command":"npm test"}"#.into()),
                    truncated: false,
                },
            },
            ChatEntry {
                id: "2:200".into(),
                at_ms: None,
                item: ChatItem::ToolResult {
                    call: Some("toolu_1".into()),
                    text: "3 passed".into(),
                    error: true,
                    truncated: true,
                },
            },
            ChatEntry {
                id: "2:300".into(),
                at_ms: None,
                item: ChatItem::Agent {
                    text: "Done.".into(),
                    truncated: true,
                },
            },
        ]
    }

    #[test]
    fn session_append_golden() {
        let id = Uuid::new_v4();
        let msg = ServerMsg::SessionAppend {
            terminal: id,
            gen: 2,
            reset: true,
            session: Some("s1".into()),
            items: sample_entries(),
            before: Some(12),
        };
        let b = body(encode_msg(&msg, None).unwrap());
        assert_eq!(
            String::from_utf8(b.clone()).unwrap(),
            format!(
                concat!(
                    r#"{{"t":"session.append","terminal":"{}","gen":2,"reset":true,"session":"s1","items":["#,
                    r#"{{"id":"2:0","atMs":1700000000000,"kind":"user","text":"fix it"}},"#,
                    r#"{{"id":"2:90.1","kind":"tool-call","call":"toolu_1","name":"Bash","summary":"npm test","input":"{{\"command\":\"npm test\"}}"}},"#,
                    r#"{{"id":"2:200","kind":"tool-result","call":"toolu_1","text":"3 passed","error":true,"truncated":true}},"#,
                    r#"{{"id":"2:300","kind":"agent","text":"Done.","truncated":true}}],"before":12}}"#
                ),
                id
            )
        );
        assert_eq!(decode_server(&b).unwrap(), msg);
        // A plain append: no `before`, and an unlinked session is `null`.
        let msg = ServerMsg::SessionAppend {
            terminal: id,
            gen: 3,
            reset: false,
            session: None,
            items: vec![],
            before: None,
        };
        let b = body(encode_msg(&msg, None).unwrap());
        assert_eq!(
            String::from_utf8(b.clone()).unwrap(),
            format!(
                r#"{{"t":"session.append","terminal":"{id}","gen":3,"reset":false,"session":null,"items":[]}}"#
            )
        );
        assert_eq!(decode_server(&b).unwrap(), msg);
    }

    #[test]
    fn session_page_golden() {
        let page = SessionPage {
            gen: 1,
            session: None,
            items: vec![],
            before: None,
        };
        assert_eq!(
            serde_json::to_string(&page).unwrap(),
            r#"{"gen":1,"session":null,"items":[]}"#
        );
        let page = SessionPage {
            gen: 4,
            session: Some("abc".into()),
            items: sample_entries(),
            before: Some(99),
        };
        let v = serde_json::to_value(&page).unwrap();
        assert_eq!(v["before"], json!(99));
        assert_eq!(serde_json::from_value::<SessionPage>(v).unwrap(), page);
    }

    #[test]
    fn chat_entries_lenient() {
        let id = Uuid::new_v4();
        let items = json!([
            {"id":"1:0","kind":"user","text":"kept"},
            {"id":"1:1","kind":"hologram","text":"newer daemon"},
            {"id":"1:2","kind":"agent"},
            7,
            {"kind":"agent","text":"no id"},
            {"id":"1:3","kind":"tool-result","text":"out","future":true},
        ]);
        let raw = json!({"t":"session.append","terminal":id,"gen":1,"reset":false,
                         "session":"s","items":items});
        let ServerMsg::SessionAppend { items: got, .. } =
            decode_server(raw.to_string().as_bytes()).unwrap()
        else {
            panic!()
        };
        let ids: Vec<_> = got.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["1:0", "1:3"]);
        assert_eq!(
            got[1].item,
            ChatItem::ToolResult {
                call: None,
                text: "out".into(),
                error: false,
                truncated: false
            }
        );
        let page: SessionPage =
            serde_json::from_value(json!({"gen":1,"session":null,"items":items,"before":5}))
                .unwrap();
        assert_eq!(page.items, got);
        assert_eq!(page.before, Some(5));
        // `items` missing or null reads as none.
        let page: SessionPage =
            serde_json::from_value(json!({"gen":1,"session":null,"items":null})).unwrap();
        assert!(page.items.is_empty());
    }

    #[test]
    fn session_append_is_known_server_type() {
        assert!(SERVER_TYPES.contains(&"session.append"));
        // A bad known message is Invalid (answerable), an unknown type is UnknownType.
        let raw = json!({"t":"session.append","gen":1});
        assert!(matches!(
            decode_server(raw.to_string().as_bytes()),
            Err(DecodeError::Invalid { .. })
        ));
        assert!(matches!(
            decode_server(br#"{"t":"session.future"}"#),
            Err(DecodeError::UnknownType { .. })
        ));
        for t in ["session.subscribe", "session.page", "session.unsubscribe"] {
            assert!(CLIENT_TYPES.contains(&t), "{t}");
        }
    }

    #[test]
    fn past_session_serde_shape() {
        let raw = json!({
            "sessions": [
                {"id": "11111111-2222", "agent": "claude", "title": "fix the bug",
                 "modifiedMs": 1_767_398_400_000u64, "messageCount": 3, "gitBranch": "main"},
                {"id": "0199aaaa", "agent": "codex", "title": "",
                 "modifiedMs": 1_767_398_300_000u64, "messageCount": 0}
            ],
            "next": {"modifiedMs": 1_767_398_300_000u64, "agent": "codex", "id": "0199aaaa"}
        });
        let page: PastSessionsPage = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(page.sessions.len(), 2);
        assert_eq!(page.sessions[0].git_branch.as_deref(), Some("main"));
        assert_eq!(page.sessions[1].git_branch, None);
        assert_eq!(page.next, Some(page.sessions[1].cursor()));
        // Round trip: the same JSON, `gitBranch` left out when absent.
        assert_eq!(serde_json::to_value(&page).unwrap(), raw);
        // The last page.
        let last: PastSessionsPage =
            serde_json::from_value(json!({"sessions": [], "next": null})).unwrap();
        assert_eq!(last.next, None);
        assert_eq!(
            serde_json::to_value(&last).unwrap(),
            json!({"sessions": [], "next": null})
        );
    }
}
