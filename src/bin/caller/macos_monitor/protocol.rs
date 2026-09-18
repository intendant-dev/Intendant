//! Private, versioned pipe protocol. Window identities carry explicit PIDs; monitor selectors never adopt
//! native display IDs. Length limits apply before JSON allocation/deserialization.

use super::controls::{ActionResult, Control, ElementAction};
use super::placement::{Bounds, Candidate, PlacementResult, WindowIdentity};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};

pub(super) const HELPER_ARG: &str = "--private-macos-monitor-helper-v1";
pub(super) const MAX_LINE: usize = 16 * 1024;
pub(super) const VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Request {
    pub seq: u64,
    pub op: Operation,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Operation {
    Create {
        width: u32,
        height: u32,
    },
    Resolve {
        handle: u32,
    },
    Destroy {
        handle: u32,
    },
    ListWindows {
        pid: i32,
    },
    BindWindow {
        handle: u32,
        identity: WindowIdentity,
        candidate: String,
    },
    PlaceWindow {
        binding: u32,
        bounds: Bounds,
    },
    ReadWindowElements {
        binding: u32,
    },
    ActWindowElement {
        binding: u32,
        element: String,
        action: ElementAction,
    },
    UnbindWindow {
        binding: u32,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Reply {
    pub seq: u64,
    pub result: Outcome,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Outcome {
    Ready {
        version: u32,
    },
    Monitor {
        handle: u32,
        native_id: u32,
        width: u32,
        height: u32,
    },
    Destroyed {
        handle: u32,
    },
    Windows {
        candidates: Vec<Candidate>,
    },
    BoundWindow {
        binding: u32,
    },
    PlacedWindow {
        result: PlacementResult,
    },
    WindowElements {
        controls: Vec<Control>,
    },
    ActedWindowElement {
        result: ActionResult,
    },
    UnboundWindow {
        binding: u32,
    },
    Error {
        message: String,
        fatal: bool,
    },
}

pub(super) fn encode(value: &impl Serialize) -> Result<Vec<u8>, String> {
    let mut line = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    if line.len() >= MAX_LINE {
        return Err("monitor protocol line too long".into());
    }
    line.push(b'\n');
    Ok(line)
}

pub(super) fn read_line(reader: &mut impl BufRead) -> Result<Option<Vec<u8>>, String> {
    let mut line = Vec::new();
    loop {
        let bytes = reader.fill_buf().map_err(|e| e.to_string())?;
        if bytes.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Err("truncated monitor protocol line".into())
            };
        }
        let n = bytes
            .iter()
            .position(|b| *b == b'\n')
            .map_or(bytes.len(), |n| n + 1);
        if line.len() + n > MAX_LINE {
            return Err("monitor protocol line too long".into());
        }
        line.extend_from_slice(&bytes[..n]);
        reader.consume(n);
        if line.last() == Some(&b'\n') {
            return Ok(Some(line));
        }
    }
}

pub(super) fn write_reply(
    writer: &mut impl Write,
    seq: u64,
    result: Outcome,
) -> Result<(), String> {
    writer
        .write_all(&encode(&Reply { seq, result })?)
        .map_err(|e| e.to_string())?;
    writer.flush().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_framing_and_strict_schema() {
        assert!(read_line(&mut &vec![b'x'; MAX_LINE + 1][..]).is_err());
        assert!(read_line(&mut &b"{}"[..]).is_err());
        assert!(read_line(&mut &b""[..]).unwrap().is_none());
        for json in [
            r#"{"seq":1,"op":{"op":"bind_window","handle":1,"identity":{"pid":1,"start_seconds":1,"start_micros":0,"window_id":1}}}"#,
            r#"{"seq":1,"op":{"op":"destroy","handle":1,"pid":42}}"#,
            r#"{"seq":1,"op":{"op":"resolve","native_id":42}}"#,
            r#"{"seq":1,"op":{"op":"create","width":-1,"height":64}}"#,
            r#"{"seq":1,"op":{"op":"adopt","handle":1}}"#,
        ] {
            assert!(serde_json::from_str::<Request>(json).is_err());
        }
        let encoded = encode(&Request {
            seq: 1,
            op: Operation::Create {
                width: 640,
                height: 480,
            },
        })
        .unwrap();
        let bytes = read_line(&mut encoded.as_slice()).unwrap().unwrap();
        assert_eq!(serde_json::from_slice::<Request>(&bytes).unwrap().seq, 1);
    }
    #[test]
    fn window_protocol_has_bounded_candidate_inventory_and_strict_selectors() {
        let candidates = (0..super::super::placement::MAX_CANDIDATES)
            .map(|n| Candidate {
                candidate: format!("macos_candidate:{n:032x}"),
                identity: WindowIdentity {
                    pid: i32::MAX,
                    start_seconds: u64::MAX,
                    start_micros: 999999,
                    window_id: n as u32 + 1,
                },
                bounds: Bounds {
                    x: -1_000_000.0,
                    y: 1_000_000.0,
                    width: 16384.0,
                    height: 16384.0,
                },
            })
            .collect();
        assert!(
            encode(&Reply {
                seq: u64::MAX,
                result: Outcome::Windows { candidates }
            })
            .unwrap()
            .len()
                < MAX_LINE
        );
        for json in [
            r#"{"seq":1,"op":{"op":"bind_window","native_id":42,"identity":{"pid":1,"start_seconds":1,"start_micros":0,"window_id":1}}}"#,
            r#"{"seq":1,"op":{"op":"bind_window","handle":1,"identity":{"pid":1,"window_id":1}}}"#,
            r#"{"seq":1,"op":{"op":"place_window","binding":1,"bounds":{"x":0,"y":0,"width":100,"height":100},"activate":true}}"#,
        ] {
            assert!(serde_json::from_str::<Request>(json).is_err());
        }
        let encoded = encode(&Request {
            seq: 1,
            op: Operation::BindWindow {
                handle: 1,
                identity: super::super::placement::tests::identity(),
                candidate: "macos_candidate:00000000000000000000000000000001".into(),
            },
        })
        .unwrap();
        let decoded = serde_json::from_slice::<Request>(&encoded).unwrap();
        assert!(
            matches!(decoded.op, Operation::BindWindow { candidate, .. } if candidate == "macos_candidate:00000000000000000000000000000001")
        );
    }
    #[test]
    fn semantic_protocol_requires_both_tokens_and_strict_tagged_actions() {
        for op in [
            serde_json::json!({"op":"read_window_elements"}),
            serde_json::json!({"op":"act_window_element","binding":1,"action":{"type":"press"}}),
            serde_json::json!({"op":"act_window_element","element":"token","action":{"type":"press"}}),
            serde_json::json!({"op":"act_window_element","binding":1,"element":"token","action":{"type":"press","text":"ignored"}}),
            serde_json::json!({"op":"act_window_element","binding":1,"element":"token","action":{"type":"set_value"}}),
            serde_json::json!({"op":"act_window_element","binding":1,"element":"token","action":{"type":"activate"}}),
            serde_json::json!({"op":"act_window_element","binding":1,"element":"token","action":{"type":"press"},"activate":true}),
        ] {
            assert!(
                serde_json::from_value::<Request>(serde_json::json!({"seq":1,"op":op})).is_err()
            );
        }
        for action in [
            ElementAction::Press {},
            ElementAction::SetValue {
                text: "\u{0001}".repeat(super::super::controls::MAX_TEXT),
            },
        ] {
            let request = Request {
                seq: u64::MAX,
                op: Operation::ActWindowElement {
                    binding: u32::MAX,
                    element: "macos_element:00000000000000000000000000000001".into(),
                    action,
                },
            };
            let wire = encode(&request).unwrap();
            assert!(wire.len() <= MAX_LINE);
            assert!(serde_json::from_slice::<Request>(&wire).is_ok());
        }
    }
}
