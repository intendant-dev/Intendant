//! Private, versioned pipe protocol. Neither side accepts a PID or an adopted
//! native display ID. Length limits apply before JSON allocation/deserialization.

use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};

pub(super) const HELPER_ARG: &str = "--private-macos-monitor-helper-v1";
pub(super) const MAX_LINE: usize = 4096;
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
    Create { width: u32, height: u32 },
    Resolve { handle: u32 },
    Destroy { handle: u32 },
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
}
