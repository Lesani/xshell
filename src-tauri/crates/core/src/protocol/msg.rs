//! Kind-0 JSON messages. Every message is an object with a string `"t"` (its type); fields are
//! camelCase and unknown fields are ignored, so protocol changes stay additive.

use super::frame::{encode_json, FrameError};
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
    #[serde(rename = "daemon.upgrade")]
    DaemonUpgrade,
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
    "daemon.upgrade",
];

/// `term.open`'s spec: a launch spec plus the Desktop-chosen UUID, initial size and opaque
/// display metadata (title, projectName, createdAt…).
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct OpenSpec {
    pub terminal: Uuid,
    #[serde(flatten)]
    pub launch: LaunchSpec,
    pub cols: u16,
    pub rows: u16,
    #[serde(default)]
    pub meta: Map<String, Value>,
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
    /// Connection-level failure; the Host closes the connection after it.
    #[serde(rename = "error")]
    Error { code: String, message: String },
}

const SERVER_TYPES: &[&str] = &["hello", "res", "terminals", "term.exit", "error"];

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
    use crate::protocol::frame::{read_frame, Frame, MAX_FRAME_LEN};
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
                }
            }
        );
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
}
