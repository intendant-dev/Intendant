//! Fixed-shape JSON envelopes for the realtime voice media send paths.
//!
//! Base64 payloads (mic chunks, video frames — frames run to hundreds of
//! KB) are spliced verbatim behind the `json_safe_base64` gate, so the hot
//! path pays one copy (the envelope `String`) instead of a
//! `serde_json::Value` copy plus a full escape-scanning `to_string` per
//! send. Non-base64 input (a caller bug) falls back to full serde
//! escaping. Each `format!` template has a serde-built twin; the tests
//! below pin the two to the same parsed `Value`, so the templates cannot
//! drift from the canonical shape.

use crate::json_safe_base64;

/// JSON string literal (quoted + escaped) for a small dynamic string —
/// frame labels come from arbitrary caller input and always go through
/// full escaping.
fn json_str(s: &str) -> String {
    // Serializing a str to JSON cannot practically fail; the fallback
    // keeps the envelope well-formed regardless.
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

/// Gemini Live video frame: `client_content` carrying the JPEG and its
/// `[frame:<id>]` annotation atomically, without ending the user turn.
pub(crate) fn gemini_frame(base64_jpeg: &str, frame_id: &str) -> String {
    if json_safe_base64(base64_jpeg) {
        let label = json_str(&format!("[frame:{frame_id}]"));
        format!(
            r#"{{"client_content":{{"turns":[{{"role":"user","parts":[{{"inlineData":{{"mimeType":"image/jpeg","data":"{base64_jpeg}"}}}},{{"text":{label}}}]}}],"turn_complete":false}}}}"#
        )
    } else {
        gemini_frame_serde(base64_jpeg, frame_id)
    }
}

fn gemini_frame_serde(base64_jpeg: &str, frame_id: &str) -> String {
    serde_json::json!({
        "client_content": {
            "turns": [{
                "role": "user",
                "parts": [
                    { "inlineData": { "mimeType": "image/jpeg", "data": base64_jpeg } },
                    { "text": format!("[frame:{frame_id}]") }
                ]
            }],
            "turn_complete": false
        }
    })
    .to_string()
}

/// Gemini Live mic chunk: one `realtime_input` media chunk of raw PCM at
/// the negotiated sample rate.
pub(crate) fn gemini_audio(base64_pcm: &str, sample_rate: u32) -> String {
    if json_safe_base64(base64_pcm) {
        format!(
            r#"{{"realtime_input":{{"media_chunks":[{{"mime_type":"audio/pcm;rate={sample_rate}","data":"{base64_pcm}"}}]}}}}"#
        )
    } else {
        gemini_audio_serde(base64_pcm, sample_rate)
    }
}

fn gemini_audio_serde(base64_pcm: &str, sample_rate: u32) -> String {
    serde_json::json!({
        "realtime_input": {
            "media_chunks": [{
                "mime_type": format!("audio/pcm;rate={sample_rate}"),
                "data": base64_pcm
            }]
        }
    })
    .to_string()
}

/// OpenAI Realtime video frame: `conversation.item.create` with the
/// `[frame:<id>]` label and the JPEG as a data-URL image content part.
pub(crate) fn openai_frame(base64_jpeg: &str, frame_id: &str) -> String {
    if json_safe_base64(base64_jpeg) {
        let label = json_str(&format!("[frame:{frame_id}]"));
        format!(
            r#"{{"type":"conversation.item.create","item":{{"type":"message","role":"user","content":[{{"type":"input_text","text":{label}}},{{"type":"input_image","image_url":"data:image/jpeg;base64,{base64_jpeg}"}}]}}}}"#
        )
    } else {
        openai_frame_serde(base64_jpeg, frame_id)
    }
}

fn openai_frame_serde(base64_jpeg: &str, frame_id: &str) -> String {
    serde_json::json!({
        "type": "conversation.item.create",
        "item": {
            "type": "message",
            "role": "user",
            "content": [
                { "type": "input_text", "text": format!("[frame:{frame_id}]") },
                {
                    "type": "input_image",
                    "image_url": format!("data:image/jpeg;base64,{base64_jpeg}")
                }
            ]
        }
    })
    .to_string()
}

/// OpenAI Realtime mic chunk: `input_audio_buffer.append`.
pub(crate) fn openai_audio(base64_pcm: &str) -> String {
    if json_safe_base64(base64_pcm) {
        format!(r#"{{"type":"input_audio_buffer.append","audio":"{base64_pcm}"}}"#)
    } else {
        openai_audio_serde(base64_pcm)
    }
}

fn openai_audio_serde(base64_pcm: &str) -> String {
    serde_json::json!({
        "type": "input_audio_buffer.append",
        "audio": base64_pcm
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> serde_json::Value {
        serde_json::from_str(s).unwrap_or_else(|e| panic!("envelope must be valid JSON: {e}\n{s}"))
    }

    /// A payload the splice gate accepts (all base64 alphabets exercised).
    const SAFE: &str = "AZaz09+/=-_";
    /// A payload that must route to the serde fallback.
    const UNSAFE: &str = "he\"llo\\wor\nld";

    /// Each `format!` template must parse to exactly the `Value` its serde
    /// twin produces for the same input (byte identity doesn't hold —
    /// serde sorts object keys).
    #[test]
    fn fast_paths_match_their_serde_twins() {
        assert_eq!(
            parse(&gemini_frame(SAFE, "cam0-f00047")),
            parse(&gemini_frame_serde(SAFE, "cam0-f00047"))
        );
        assert_eq!(
            parse(&gemini_audio(SAFE, 16000)),
            parse(&gemini_audio_serde(SAFE, 16000))
        );
        assert_eq!(
            parse(&openai_frame(SAFE, "cam0-f00047")),
            parse(&openai_frame_serde(SAFE, "cam0-f00047"))
        );
        assert_eq!(parse(&openai_audio(SAFE)), parse(&openai_audio_serde(SAFE)));
    }

    /// Frame labels are arbitrary caller strings: the fast path must still
    /// escape them (only the base64 payload is spliced raw).
    #[test]
    fn frame_labels_are_escaped_on_the_fast_path() {
        let tricky = "id\"with\\quotes";
        assert_eq!(
            parse(&gemini_frame(SAFE, tricky)),
            parse(&gemini_frame_serde(SAFE, tricky))
        );
        assert_eq!(
            parse(&openai_frame(SAFE, tricky)),
            parse(&openai_frame_serde(SAFE, tricky))
        );
    }

    /// Non-base64 payloads route to the fallback and survive intact.
    #[test]
    fn unsafe_payloads_fall_back_and_round_trip() {
        let v = parse(&gemini_frame(UNSAFE, "f1"));
        assert_eq!(
            v["client_content"]["turns"][0]["parts"][0]["inlineData"]["data"],
            UNSAFE
        );
        let v = parse(&gemini_audio(UNSAFE, 24000));
        assert_eq!(
            v["realtime_input"]["media_chunks"][0]["data"], UNSAFE,
            "payload must survive the fallback"
        );
        assert_eq!(
            v["realtime_input"]["media_chunks"][0]["mime_type"],
            "audio/pcm;rate=24000"
        );
        let v = parse(&openai_frame(UNSAFE, "f1"));
        assert_eq!(
            v["item"]["content"][1]["image_url"],
            format!("data:image/jpeg;base64,{UNSAFE}")
        );
        let v = parse(&openai_audio(UNSAFE));
        assert_eq!(v["audio"], UNSAFE);
    }
}
