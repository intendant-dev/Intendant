//! Managed-context support: request/context payload snapshots read from the
//! codex trace archive, trace-bundle discovery and fingerprinting, context
//! summaries, and token-usage extraction from traced inference payloads.

use super::*;

pub(crate) struct CodexRequestPayloadSnapshot {
    pub(crate) label: String,
    pub(crate) request_id: String,
    pub(crate) request_index: u64,
    pub(crate) format: String,
    pub(crate) payload: serde_json::Value,
}

#[derive(Clone)]
pub(crate) struct CodexRequestPayloadRef {
    pub(crate) bundle_dir: PathBuf,
    pub(crate) relative_path: String,
    pub(crate) inference_call_id: String,
    pub(crate) thread_id: Option<String>,
    pub(crate) provider_name: Option<String>,
    pub(crate) order: (i64, u64),
}

#[derive(Clone)]
pub(crate) struct CodexResponsePayloadRef {
    pub(crate) bundle_dir: PathBuf,
    pub(crate) relative_path: String,
    pub(crate) inference_call_id: String,
    pub(crate) response_id: String,
}

pub(crate) struct CodexTraceIndex {
    pub(crate) requests: Vec<CodexRequestPayloadRef>,
    pub(crate) requests_by_call: HashMap<String, CodexRequestPayloadRef>,
    pub(crate) responses_by_id: HashMap<String, CodexResponsePayloadRef>,
}

#[allow(dead_code)]
pub(crate) async fn read_latest_codex_request_payload(
    root: &Path,
) -> Result<CodexRequestPayloadSnapshot, CallerError> {
    read_latest_codex_context_payload(root, None).await
}

pub(crate) fn codex_context_snapshot_not_ready(err: &CallerError) -> bool {
    matches!(
        err,
        CallerError::ExternalAgent(message)
            if message.starts_with("no Codex inference request payload found in ")
    )
}

pub(crate) async fn read_latest_codex_context_payload(
    root: &Path,
    thread_id: Option<&str>,
) -> Result<CodexRequestPayloadSnapshot, CallerError> {
    let snapshots = read_codex_context_payloads(root, thread_id).await?;
    snapshots.into_iter().last().ok_or_else(|| {
        CallerError::ExternalAgent(format!(
            "no Codex inference request payload found in {}",
            root.display()
        ))
    })
}

pub(crate) async fn read_codex_context_payloads(
    root: &Path,
    thread_id: Option<&str>,
) -> Result<Vec<CodexRequestPayloadSnapshot>, CallerError> {
    read_codex_context_payloads_excluding(root, thread_id, &HashSet::new()).await
}

pub(crate) fn context_snapshots_from_trace_archive(
    root: &Path,
    thread_id: &str,
    exact_archive: bool,
) -> Result<Vec<AgentContextSnapshot>, CallerError> {
    let traces = read_codex_context_payloads_sync(root, Some(thread_id))?;
    Ok(traces
        .into_iter()
        .map(|trace| {
            let item_count = codex_request_item_count(&trace.payload);
            let raw = codex_context_archive_payload(
                trace.payload,
                &trace.request_id,
                trace.request_index,
                &trace.format,
                exact_archive,
            );
            AgentContextSnapshot {
                source: "codex".to_string(),
                label: trace.label,
                request_id: Some(trace.request_id),
                request_index: Some(trace.request_index),
                rollout_path: None,
                format: trace.format,
                token_count: None,
                token_count_kind: None,
                context_window: None,
                hard_context_window: None,
                item_count,
                raw,
            }
        })
        .collect())
}

pub(crate) fn read_codex_context_payloads_sync(
    root: &Path,
    thread_id: Option<&str>,
) -> Result<Vec<CodexRequestPayloadSnapshot>, CallerError> {
    let index = read_codex_trace_index_sync(root, thread_id)?;
    let mut requests = index.requests.clone();
    requests.sort_by_key(codex_request_sort_key);

    let mut snapshots = Vec::with_capacity(requests.len());
    for (idx, request_ref) in requests.iter().enumerate() {
        snapshots.push(codex_context_payload_snapshot_sync(
            &index,
            request_ref,
            idx as u64 + 1,
        )?);
    }
    Ok(snapshots)
}

pub(crate) async fn read_codex_context_payloads_excluding(
    root: &Path,
    thread_id: Option<&str>,
    seen_request_ids: &HashSet<String>,
) -> Result<Vec<CodexRequestPayloadSnapshot>, CallerError> {
    let index = read_codex_trace_index(root, thread_id).await?;
    let mut requests = index.requests.clone();
    requests.sort_by_key(codex_request_sort_key);

    let mut snapshots = Vec::with_capacity(requests.len());
    for (idx, request_ref) in requests.iter().enumerate() {
        if seen_request_ids.contains(&codex_request_id(request_ref)) {
            continue;
        }
        snapshots.push(codex_context_payload_snapshot(&index, request_ref, idx as u64 + 1).await?);
    }
    Ok(snapshots)
}

pub(crate) async fn codex_context_payload_snapshot(
    index: &CodexTraceIndex,
    request_ref: &CodexRequestPayloadRef,
    request_index: u64,
) -> Result<CodexRequestPayloadSnapshot, CallerError> {
    let payload =
        read_codex_json_payload(&request_ref.bundle_dir, &request_ref.relative_path).await?;
    let format = codex_request_format(request_ref.provider_name.as_deref());
    let request_id = codex_request_id(request_ref);
    if format == "openai.responses.request.v1" {
        let resolved =
            resolve_openai_responses_context_payload(index, request_ref, request_index, payload)
                .await?;
        return Ok(CodexRequestPayloadSnapshot {
            label: "Codex resolved request payload".to_string(),
            request_id,
            request_index,
            format: "openai.responses.resolved_request.v1".to_string(),
            payload: resolved,
        });
    }

    Ok(CodexRequestPayloadSnapshot {
        label: "Codex request payload".to_string(),
        request_id,
        request_index,
        format,
        payload,
    })
}

pub(crate) fn codex_context_payload_snapshot_sync(
    index: &CodexTraceIndex,
    request_ref: &CodexRequestPayloadRef,
    request_index: u64,
) -> Result<CodexRequestPayloadSnapshot, CallerError> {
    let payload =
        read_codex_json_payload_sync(&request_ref.bundle_dir, &request_ref.relative_path)?;
    let format = codex_request_format(request_ref.provider_name.as_deref());
    let request_id = codex_request_id(request_ref);
    if format == "openai.responses.request.v1" {
        let resolved = resolve_openai_responses_context_payload_sync(
            index,
            request_ref,
            request_index,
            payload,
        )?;
        return Ok(CodexRequestPayloadSnapshot {
            label: "Codex resolved request payload".to_string(),
            request_id,
            request_index,
            format: "openai.responses.resolved_request.v1".to_string(),
            payload: resolved,
        });
    }

    Ok(CodexRequestPayloadSnapshot {
        label: "Codex request payload".to_string(),
        request_id,
        request_index,
        format,
        payload,
    })
}

pub(crate) fn codex_request_sort_key(
    request: &CodexRequestPayloadRef,
) -> (i64, u64, String, String, String) {
    (
        request.order.0,
        request.order.1,
        request.bundle_dir.to_string_lossy().to_string(),
        request.relative_path.clone(),
        request.inference_call_id.clone(),
    )
}

pub(crate) fn codex_request_id(request: &CodexRequestPayloadRef) -> String {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;

    fn feed(hash: &mut u64, part: &str) {
        for byte in part.as_bytes() {
            *hash ^= u64::from(*byte);
            *hash = hash.wrapping_mul(FNV_PRIME);
        }
        *hash ^= 0xff;
        *hash = hash.wrapping_mul(FNV_PRIME);
    }

    let bundle_dir = request.bundle_dir.to_string_lossy();
    let thread_id = request.thread_id.as_deref().unwrap_or_default();
    let mut hash = FNV_OFFSET;
    feed(&mut hash, &bundle_dir);
    feed(&mut hash, &request.relative_path);
    feed(&mut hash, &request.inference_call_id);
    feed(&mut hash, thread_id);
    format!("codex-request-{hash:016x}")
}

pub(crate) fn stable_context_hash(bytes: &[u8]) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;
    let mut hash = FNV_OFFSET;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

pub(crate) fn compact_context_text(text: &str, limit: usize) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() <= limit {
        return text;
    }
    let mut out = text
        .chars()
        .take(limit.saturating_sub(3))
        .collect::<String>();
    out.push_str("...");
    out
}

pub(crate) fn context_json_len(value: &serde_json::Value) -> usize {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len())
        .unwrap_or_else(|_| value.to_string().len())
}

pub(crate) fn context_estimated_tokens(value: &serde_json::Value) -> u64 {
    let chars = context_json_len(value);
    std::cmp::max(1, chars.div_ceil(4) as u64)
}

pub(crate) fn context_first_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Array(items) => items
            .iter()
            .map(context_first_text)
            .find(|text| !text.trim().is_empty())
            .unwrap_or_default(),
        serde_json::Value::Object(map) => {
            for key in [
                "text",
                "input_text",
                "output_text",
                "summary",
                "content",
                "output",
                "arguments",
            ] {
                if let Some(serde_json::Value::String(text)) = map.get(key) {
                    if !text.trim().is_empty() {
                        return text.clone();
                    }
                }
            }
            for key in ["parts", "content"] {
                if let Some(value) = map.get(key) {
                    let found = context_first_text(value);
                    if !found.trim().is_empty() {
                        return found;
                    }
                }
            }
            String::new()
        }
        _ => String::new(),
    }
}

pub(crate) fn context_has_media(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Array(items) => items.iter().any(context_has_media),
        serde_json::Value::Object(map) => {
            let type_text = map
                .get("type")
                .or_else(|| map.get("mime_type"))
                .or_else(|| map.get("mimeType"))
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_ascii_lowercase();
            if ["image", "audio", "video", "file"]
                .iter()
                .any(|needle| type_text.contains(needle))
            {
                return true;
            }
            if [
                "image_url",
                "input_image",
                "inline_data",
                "inlineData",
                "media",
            ]
            .iter()
            .any(|key| map.contains_key(*key))
            {
                return true;
            }
            map.values().any(context_has_media)
        }
        _ => false,
    }
}

pub(crate) fn context_message_category(item: &serde_json::Value) -> &'static str {
    let role = item
        .get("role")
        .or_else(|| item.get("speaker"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let item_type = item
        .get("type")
        .or_else(|| item.get("kind"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if item_type.contains("reasoning") || item_type.contains("thinking") {
        return "reasoning";
    }
    if item_type.contains("function_call_output")
        || item_type == "tool_result"
        || item_type == "functionresponse"
    {
        return "tool_output";
    }
    if item_type.contains("function_call") || item_type == "tool_use" || item_type == "functioncall"
    {
        return "tool_call";
    }
    match role.as_str() {
        "system" | "developer" => "instructions",
        "user" | "human" => {
            if context_has_media(item) {
                "media"
            } else {
                "user"
            }
        }
        "assistant" | "model" => {
            if context_has_media(item) {
                "media"
            } else {
                "assistant"
            }
        }
        "tool" => "tool_output",
        _ if context_has_media(item) => "media",
        _ => "other",
    }
}

pub(crate) fn context_message_title(item: &serde_json::Value, index: usize) -> String {
    if let Some(name) = item.get("name").and_then(|v| v.as_str()) {
        return name.to_string();
    }
    if let Some(name) = item
        .pointer("/function/name")
        .and_then(|v| v.as_str())
        .or_else(|| item.pointer("/tool/name").and_then(|v| v.as_str()))
    {
        return name.to_string();
    }
    let role = item
        .get("role")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let item_type = item
        .get("type")
        .or_else(|| item.get("kind"))
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    match (role.is_empty(), item_type.is_empty()) {
        (false, false) => format!("{role} {item_type}"),
        (false, true) => format!("{role} message"),
        (true, false) => item_type.replace('_', " "),
        (true, true) => format!("item {}", index + 1),
    }
}

pub(crate) fn context_tool_name(tool: &serde_json::Value, fallback_index: usize) -> String {
    tool.pointer("/function/name")
        .and_then(|v| v.as_str())
        .or_else(|| tool.get("name").and_then(|v| v.as_str()))
        .or_else(|| tool.pointer("/tool/name").and_then(|v| v.as_str()))
        .map(str::to_string)
        .unwrap_or_else(|| format!("tool {}", fallback_index + 1))
}

pub(crate) fn push_context_summary_part(
    parts: &mut Vec<serde_json::Value>,
    category: &str,
    title: impl Into<String>,
    value: &serde_json::Value,
    path: impl Into<String>,
) {
    let first_text = context_first_text(value);
    let preview = if first_text.trim().is_empty() {
        compact_context_text(&value.to_string(), 360)
    } else {
        compact_context_text(&first_text, 360)
    };
    parts.push(serde_json::json!({
        "category": category,
        "title": title.into(),
        "subtitle": compact_context_text(&first_text, 180),
        "path": path.into(),
        "preview": preview,
        "estimated_tokens": context_estimated_tokens(value),
        "chars": context_json_len(value),
    }));
}

pub(crate) fn codex_context_summary_parts(payload: &serde_json::Value) -> Vec<serde_json::Value> {
    let mut parts = Vec::new();
    let mut consumed = HashSet::new();
    if let Some(map) = payload.as_object() {
        for key in [
            "instructions",
            "system",
            "system_instruction",
            "developer",
            "developer_message",
        ] {
            if let Some(value) = map.get(key) {
                consumed.insert(key);
                push_context_summary_part(
                    &mut parts,
                    "instructions",
                    key.replace('_', " "),
                    value,
                    format!("$.{key}"),
                );
            }
        }
        if let Some(tools) = map.get("tools").and_then(|v| v.as_array()) {
            consumed.insert("tools");
            for (index, tool) in tools.iter().enumerate() {
                push_context_summary_part(
                    &mut parts,
                    "schema",
                    format!("tool schema: {}", context_tool_name(tool, index)),
                    tool,
                    format!("$.tools[{index}]"),
                );
            }
        }
        for key in ["input", "messages", "contents", "history", "output_items"] {
            if let Some(items) = map.get(key).and_then(|v| v.as_array()) {
                consumed.insert(key);
                for (index, item) in items.iter().enumerate() {
                    push_context_summary_part(
                        &mut parts,
                        context_message_category(item),
                        context_message_title(item, index),
                        item,
                        format!("$.{key}[{index}]"),
                    );
                }
            }
        }
        let mut config = serde_json::Map::new();
        for (key, value) in map {
            if consumed.contains(key.as_str()) || value.is_null() {
                continue;
            }
            if value.is_string()
                || value.is_number()
                || value.is_boolean()
                || matches!(
                    key.as_str(),
                    "reasoning" | "metadata" | "include" | "tool_choice"
                )
            {
                config.insert(key.clone(), value.clone());
            }
        }
        if !config.is_empty() {
            push_context_summary_part(
                &mut parts,
                "config",
                "request configuration",
                &serde_json::Value::Object(config),
                "$.config",
            );
        }
    } else if let Some(items) = payload.as_array() {
        for (index, item) in items.iter().enumerate() {
            push_context_summary_part(
                &mut parts,
                context_message_category(item),
                context_message_title(item, index),
                item,
                format!("$[{index}]"),
            );
        }
    }
    if parts.is_empty() {
        push_context_summary_part(&mut parts, "other", "raw context payload", payload, "$");
    }
    parts
}

pub(crate) fn codex_context_archive_payload(
    payload: serde_json::Value,
    request_id: &str,
    request_index: u64,
    format: &str,
    exact: bool,
) -> serde_json::Value {
    let raw_bytes = serde_json::to_vec(&payload).unwrap_or_else(|_| payload.to_string().into());
    let raw_len = raw_bytes.len();
    let raw_hash = format!("{:016x}", stable_context_hash(&raw_bytes));
    if exact {
        let mut payload = payload;
        if let serde_json::Value::Object(map) = &mut payload {
            let context = map
                .entry("_intendant_context".to_string())
                .or_insert_with(|| serde_json::json!({}));
            if let serde_json::Value::Object(context_map) = context {
                context_map.insert("archive_mode".to_string(), serde_json::json!("exact"));
                context_map.insert("raw_archived".to_string(), serde_json::json!(true));
                context_map.insert("raw_bytes".to_string(), serde_json::json!(raw_len));
                context_map.insert("raw_hash".to_string(), serde_json::json!(raw_hash));
                context_map.insert("request_id".to_string(), serde_json::json!(request_id));
                context_map.insert(
                    "request_index".to_string(),
                    serde_json::json!(request_index),
                );
            }
        }
        return payload;
    }
    let summary_parts = codex_context_summary_parts(&payload);
    serde_json::json!({
        "_intendant_context": {
            "archive_mode": "summary",
            "raw_archived": false,
            "raw_bytes": raw_len,
            "raw_hash": raw_hash,
            "request_id": request_id,
            "request_index": request_index,
            "format": format,
        },
        "summary": {
            "kind": "compact_context_snapshot",
            "raw_bytes": raw_len,
            "part_count": summary_parts.len(),
            "exact_replay_available": false,
        },
        "summary_parts": summary_parts,
    })
}

pub(crate) async fn read_codex_trace_index(
    root: &Path,
    thread_id: Option<&str>,
) -> Result<CodexTraceIndex, CallerError> {
    let bundle_dirs = collect_codex_trace_bundle_dirs(root, thread_id).await?;
    let mut requests = Vec::new();
    let mut requests_by_call = HashMap::new();
    let mut responses_by_id = HashMap::new();

    for bundle_dir in bundle_dirs {
        let trace_path = bundle_dir.join("trace.jsonl");
        let contents = match tokio::fs::read_to_string(&trace_path).await {
            Ok(contents) => contents,
            Err(_) => continue,
        };

        for (line_idx, line) in contents.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if let Some(candidate) =
                codex_inference_request_ref(&bundle_dir, &event, line_idx as u64)
            {
                if thread_id
                    .zip(candidate.thread_id.as_deref())
                    .map(|(expected, actual)| expected != actual)
                    .unwrap_or(false)
                {
                    continue;
                }
                requests_by_call.insert(codex_trace_call_key(&candidate), candidate.clone());
                requests.push(candidate);
                continue;
            }
            if let Some(response) = codex_inference_response_ref(&bundle_dir, &event) {
                responses_by_id.insert(response.response_id.clone(), response);
            }
        }
    }

    Ok(CodexTraceIndex {
        requests,
        requests_by_call,
        responses_by_id,
    })
}

pub(crate) fn read_codex_trace_index_sync(
    root: &Path,
    thread_id: Option<&str>,
) -> Result<CodexTraceIndex, CallerError> {
    let bundle_dirs = collect_codex_trace_bundle_dirs_sync(root, thread_id)?;
    let mut requests = Vec::new();
    let mut requests_by_call = HashMap::new();
    let mut responses_by_id = HashMap::new();

    for bundle_dir in bundle_dirs {
        let trace_path = bundle_dir.join("trace.jsonl");
        let contents = match std::fs::read_to_string(&trace_path) {
            Ok(contents) => contents,
            Err(_) => continue,
        };

        for (line_idx, line) in contents.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if let Some(candidate) =
                codex_inference_request_ref(&bundle_dir, &event, line_idx as u64)
            {
                if thread_id
                    .zip(candidate.thread_id.as_deref())
                    .map(|(expected, actual)| expected != actual)
                    .unwrap_or(false)
                {
                    continue;
                }
                requests_by_call.insert(codex_trace_call_key(&candidate), candidate.clone());
                requests.push(candidate);
                continue;
            }
            if let Some(response) = codex_inference_response_ref(&bundle_dir, &event) {
                responses_by_id.insert(response.response_id.clone(), response);
            }
        }
    }

    Ok(CodexTraceIndex {
        requests,
        requests_by_call,
        responses_by_id,
    })
}

pub(crate) async fn collect_codex_trace_bundle_dirs(
    root: &Path,
    thread_id: Option<&str>,
) -> Result<Vec<PathBuf>, CallerError> {
    let mut trace_roots = vec![root.to_path_buf()];
    if thread_id.is_some() {
        if let Some(logs_root) = codex_logs_root_for_trace_root(root) {
            if let Ok(mut sessions) = tokio::fs::read_dir(&logs_root).await {
                while let Ok(Some(entry)) = sessions.next_entry().await {
                    let file_type = match entry.file_type().await {
                        Ok(file_type) => file_type,
                        Err(_) => continue,
                    };
                    if file_type.is_dir() {
                        let trace_root = entry.path().join("model-request-traces");
                        if trace_root != root {
                            trace_roots.push(trace_root);
                        }
                    }
                }
            }
        }
    }

    let mut seen_roots = HashSet::new();
    trace_roots.retain(|path| seen_roots.insert(path.clone()));

    let mut bundle_dirs = Vec::new();
    let mut seen_bundles = HashSet::new();
    for trace_root in trace_roots {
        let mut dirs = match tokio::fs::read_dir(&trace_root).await {
            Ok(dirs) => dirs,
            Err(e) if trace_root == root => {
                return Err(CallerError::ExternalAgent(format!(
                    "read Codex request trace root {}: {e}",
                    root.display()
                )));
            }
            Err(_) => continue,
        };

        while let Some(entry) = dirs.next_entry().await.map_err(|e| {
            CallerError::ExternalAgent(format!("read Codex request trace entry: {e}"))
        })? {
            let file_type = match entry.file_type().await {
                Ok(file_type) => file_type,
                Err(_) => continue,
            };
            if !file_type.is_dir() {
                continue;
            }
            let bundle_dir = entry.path();
            if let Some(thread_id) = thread_id {
                let name = bundle_dir
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or_default();
                if !name.contains(thread_id) {
                    continue;
                }
            }
            if seen_bundles.insert(bundle_dir.clone()) {
                bundle_dirs.push(bundle_dir);
            }
        }
    }

    Ok(bundle_dirs)
}

pub(crate) async fn codex_context_trace_fingerprint(
    root: &Path,
    thread_id: Option<&str>,
) -> Result<CodexTraceFingerprint, CallerError> {
    let bundle_dirs = collect_codex_trace_bundle_dirs(root, thread_id).await?;
    let mut files = Vec::new();
    for bundle_dir in bundle_dirs {
        let path = bundle_dir.join("trace.jsonl");
        let metadata = match tokio::fs::metadata(&path).await {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        files.push(CodexTraceFileFingerprint {
            path,
            len: metadata.len(),
            modified: metadata.modified().ok(),
        });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(CodexTraceFingerprint { files })
}

pub(crate) fn collect_codex_trace_bundle_dirs_sync(
    root: &Path,
    thread_id: Option<&str>,
) -> Result<Vec<PathBuf>, CallerError> {
    let entries = std::fs::read_dir(root).map_err(|e| {
        CallerError::ExternalAgent(format!(
            "read Codex request trace root {}: {e}",
            root.display()
        ))
    })?;
    let mut bundle_dirs = Vec::new();
    let mut seen_bundles = HashSet::new();
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let bundle_dir = entry.path();
        if let Some(thread_id) = thread_id {
            let name = bundle_dir
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default();
            if !name.contains(thread_id) {
                continue;
            }
        }
        if seen_bundles.insert(bundle_dir.clone()) {
            bundle_dirs.push(bundle_dir);
        }
    }

    Ok(bundle_dirs)
}

pub(crate) fn codex_logs_root_for_trace_root(root: &Path) -> Option<PathBuf> {
    if root.file_name().and_then(|name| name.to_str()) != Some("model-request-traces") {
        return None;
    }
    root.parent()?.parent().map(Path::to_path_buf)
}

pub(crate) async fn read_codex_json_payload(
    bundle_dir: &Path,
    relative_path: &str,
) -> Result<serde_json::Value, CallerError> {
    let payload_path = bundle_dir.join(relative_path);
    let contents = tokio::fs::read_to_string(&payload_path)
        .await
        .map_err(|e| {
            CallerError::ExternalAgent(format!(
                "read Codex request payload {}: {e}",
                payload_path.display()
            ))
        })?;
    serde_json::from_str::<serde_json::Value>(&contents).map_err(CallerError::Json)
}

pub(crate) fn read_codex_json_payload_sync(
    bundle_dir: &Path,
    relative_path: &str,
) -> Result<serde_json::Value, CallerError> {
    let payload_path = bundle_dir.join(relative_path);
    let contents = std::fs::read_to_string(&payload_path).map_err(|e| {
        CallerError::ExternalAgent(format!(
            "read Codex request payload {}: {e}",
            payload_path.display()
        ))
    })?;
    serde_json::from_str::<serde_json::Value>(&contents).map_err(CallerError::Json)
}

pub(crate) async fn resolve_openai_responses_context_payload(
    index: &CodexTraceIndex,
    latest_ref: &CodexRequestPayloadRef,
    request_index: u64,
    latest_payload: serde_json::Value,
) -> Result<serde_json::Value, CallerError> {
    let mut previous_pairs = Vec::new();
    let mut unresolved_previous_response_id = None;
    let mut seen_response_ids = HashSet::new();
    let mut previous_response_id = codex_previous_response_id(&latest_payload).map(str::to_string);

    while let Some(response_id) = previous_response_id {
        if !seen_response_ids.insert(response_id.clone()) {
            unresolved_previous_response_id = Some(response_id);
            break;
        }
        let Some(response_ref) = index.responses_by_id.get(&response_id).cloned() else {
            unresolved_previous_response_id = Some(response_id);
            break;
        };
        let request_key =
            codex_trace_call_key_parts(&response_ref.bundle_dir, &response_ref.inference_call_id);
        let Some(request_ref) = index.requests_by_call.get(&request_key).cloned() else {
            unresolved_previous_response_id = Some(response_id);
            break;
        };
        let request_payload =
            read_codex_json_payload(&request_ref.bundle_dir, &request_ref.relative_path).await?;
        let response_payload =
            read_codex_json_payload(&response_ref.bundle_dir, &response_ref.relative_path).await?;
        previous_response_id = codex_previous_response_id(&request_payload).map(str::to_string);
        previous_pairs.push((request_payload, response_payload));
    }

    previous_pairs.reverse();

    let mut resolved_input = Vec::new();
    for (request_payload, response_payload) in previous_pairs {
        codex_extend_array_field(&mut resolved_input, &request_payload, "input");
        codex_extend_array_field(&mut resolved_input, &response_payload, "output_items");
    }
    codex_extend_array_field(&mut resolved_input, &latest_payload, "input");

    let latest_request_input_count = latest_payload
        .get("input")
        .and_then(|input| input.as_array())
        .map(Vec::len)
        .unwrap_or(0);
    let mut resolved_payload = latest_payload;
    if let serde_json::Value::Object(map) = &mut resolved_payload {
        map.insert(
            "input".to_string(),
            serde_json::Value::Array(resolved_input.clone()),
        );
        map.insert(
            "_intendant_context".to_string(),
            serde_json::json!({
                "source": "codex_rollout_trace_payloads",
                "thread_id": latest_ref.thread_id.clone(),
                "request_id": codex_request_id(latest_ref),
                "request_index": request_index,
                "inference_call_id": latest_ref.inference_call_id.clone(),
                "latest_request_input_count": latest_request_input_count,
                "resolved_input_count": resolved_input.len(),
                "unresolved_previous_response_id": unresolved_previous_response_id,
            }),
        );
    }

    Ok(resolved_payload)
}

pub(crate) fn resolve_openai_responses_context_payload_sync(
    index: &CodexTraceIndex,
    latest_ref: &CodexRequestPayloadRef,
    request_index: u64,
    latest_payload: serde_json::Value,
) -> Result<serde_json::Value, CallerError> {
    let mut previous_pairs = Vec::new();
    let mut unresolved_previous_response_id = None;
    let mut seen_response_ids = HashSet::new();
    let mut previous_response_id = codex_previous_response_id(&latest_payload).map(str::to_string);

    while let Some(response_id) = previous_response_id {
        if !seen_response_ids.insert(response_id.clone()) {
            unresolved_previous_response_id = Some(response_id);
            break;
        }
        let Some(response_ref) = index.responses_by_id.get(&response_id).cloned() else {
            unresolved_previous_response_id = Some(response_id);
            break;
        };
        let request_key =
            codex_trace_call_key_parts(&response_ref.bundle_dir, &response_ref.inference_call_id);
        let Some(request_ref) = index.requests_by_call.get(&request_key).cloned() else {
            unresolved_previous_response_id = Some(response_id);
            break;
        };
        let request_payload =
            read_codex_json_payload_sync(&request_ref.bundle_dir, &request_ref.relative_path)?;
        let response_payload =
            read_codex_json_payload_sync(&response_ref.bundle_dir, &response_ref.relative_path)?;
        previous_response_id = codex_previous_response_id(&request_payload).map(str::to_string);
        previous_pairs.push((request_payload, response_payload));
    }

    previous_pairs.reverse();

    let mut resolved_input = Vec::new();
    for (request_payload, response_payload) in previous_pairs {
        codex_extend_array_field(&mut resolved_input, &request_payload, "input");
        codex_extend_array_field(&mut resolved_input, &response_payload, "output_items");
    }
    codex_extend_array_field(&mut resolved_input, &latest_payload, "input");

    let latest_request_input_count = latest_payload
        .get("input")
        .and_then(|input| input.as_array())
        .map(Vec::len)
        .unwrap_or(0);
    let mut resolved_payload = latest_payload;
    if let serde_json::Value::Object(map) = &mut resolved_payload {
        map.insert(
            "input".to_string(),
            serde_json::Value::Array(resolved_input.clone()),
        );
        map.insert(
            "_intendant_context".to_string(),
            serde_json::json!({
                "source": "codex_rollout_trace_payloads",
                "thread_id": latest_ref.thread_id.clone(),
                "request_id": codex_request_id(latest_ref),
                "request_index": request_index,
                "inference_call_id": latest_ref.inference_call_id.clone(),
                "latest_request_input_count": latest_request_input_count,
                "resolved_input_count": resolved_input.len(),
                "unresolved_previous_response_id": unresolved_previous_response_id,
            }),
        );
    }

    Ok(resolved_payload)
}

pub(crate) fn codex_previous_response_id(payload: &serde_json::Value) -> Option<&str> {
    payload
        .get("previous_response_id")
        .and_then(|value| value.as_str())
}

pub(crate) fn codex_extend_array_field(
    target: &mut Vec<serde_json::Value>,
    payload: &serde_json::Value,
    field: &str,
) {
    if let Some(items) = payload.get(field).and_then(|value| value.as_array()) {
        target.extend(items.iter().cloned());
    }
}

pub(crate) fn codex_trace_call_key(request: &CodexRequestPayloadRef) -> String {
    codex_trace_call_key_parts(&request.bundle_dir, &request.inference_call_id)
}

pub(crate) fn codex_trace_call_key_parts(bundle_dir: &Path, inference_call_id: &str) -> String {
    format!("{}::{inference_call_id}", bundle_dir.display())
}

pub(crate) fn codex_inference_request_ref(
    bundle_dir: &Path,
    event: &serde_json::Value,
    line_idx: u64,
) -> Option<CodexRequestPayloadRef> {
    // Codex trace schema v1 writes the event kind under `payload.type`.
    // Older traces wrapped the same payload in `{ type: "event_msg", ... }`;
    // both shapes are intentionally accepted here.
    let payload = event.get("payload")?;
    if payload.get("type").and_then(|v| v.as_str()) != Some("inference_started") {
        return None;
    }
    let request_payload = payload.get("request_payload")?;
    if request_payload
        .get("kind")?
        .get("type")
        .and_then(|v| v.as_str())?
        != "inference_request"
    {
        return None;
    }
    let relative_path = request_payload.get("path")?.as_str()?.to_string();
    Some(CodexRequestPayloadRef {
        bundle_dir: bundle_dir.to_path_buf(),
        relative_path,
        inference_call_id: payload.get("inference_call_id")?.as_str()?.to_string(),
        thread_id: payload
            .get("thread_id")
            .and_then(|v| v.as_str())
            .map(ToString::to_string),
        provider_name: payload
            .get("provider_name")
            .and_then(|v| v.as_str())
            .or_else(|| {
                request_payload
                    .get("provider_name")
                    .and_then(|v| v.as_str())
            })
            .map(ToString::to_string),
        order: (
            event
                .get("wall_time_unix_ms")
                .and_then(|v| v.as_i64())
                .or_else(|| event.get("ts").and_then(|v| v.as_i64()))
                .unwrap_or(0),
            line_idx,
        ),
    })
}

pub(crate) fn codex_inference_response_ref(
    bundle_dir: &Path,
    event: &serde_json::Value,
) -> Option<CodexResponsePayloadRef> {
    let payload = event.get("payload")?;
    if payload.get("type").and_then(|v| v.as_str()) != Some("inference_completed") {
        return None;
    }
    let response_payload = payload.get("response_payload")?;
    if response_payload
        .get("kind")?
        .get("type")
        .and_then(|v| v.as_str())?
        != "inference_response"
    {
        return None;
    }
    Some(CodexResponsePayloadRef {
        bundle_dir: bundle_dir.to_path_buf(),
        relative_path: response_payload.get("path")?.as_str()?.to_string(),
        inference_call_id: payload.get("inference_call_id")?.as_str()?.to_string(),
        response_id: payload.get("response_id")?.as_str()?.to_string(),
    })
}

pub(crate) fn codex_request_format(provider_name: Option<&str>) -> String {
    let normalized = provider_name.map(|provider| provider.to_ascii_lowercase());
    match normalized.as_deref() {
        Some("openai") => "openai.responses.request.v1".to_string(),
        Some("anthropic") => "anthropic.messages.request.v1".to_string(),
        Some("gemini") => "gemini.generate-content.request.v1".to_string(),
        Some(provider) => format!("codex.{}.inference_request_payload.v1", provider),
        None => "codex.inference_request_payload.v1".to_string(),
    }
}

pub(crate) fn first_u64_at(value: &serde_json::Value, paths: &[&str]) -> Option<u64> {
    paths
        .iter()
        .find_map(|path| value.pointer(path).and_then(|v| v.as_u64()))
}

pub(crate) fn codex_usage_bucket<'a>(
    value: &'a serde_json::Value,
    names: &[&str],
) -> Option<&'a serde_json::Value> {
    for name in names {
        if let Some(v) = value.get(*name) {
            return Some(v);
        }
        if let Some(info) = value.get("info") {
            if let Some(v) = info.get(*name) {
                return Some(v);
            }
        }
    }
    None
}

pub(crate) fn codex_usage_total_tokens(value: &serde_json::Value) -> Option<u64> {
    first_u64_at(
        value,
        &[
            "/last/totalTokens",
            "/last/total_tokens",
            "/last_token_usage/totalTokens",
            "/last_token_usage/total_tokens",
            "/total/totalTokens",
            "/total/total_tokens",
            "/total_token_usage/totalTokens",
            "/total_token_usage/total_tokens",
            "/info/last/totalTokens",
            "/info/last/total_tokens",
            "/info/last_token_usage/totalTokens",
            "/info/last_token_usage/total_tokens",
            "/info/total/totalTokens",
            "/info/total/total_tokens",
            "/info/total_token_usage/totalTokens",
            "/info/total_token_usage/total_tokens",
            "/totalTokens",
            "/total_tokens",
        ],
    )
}

pub(crate) fn codex_usage_component_tokens(value: &serde_json::Value) -> u64 {
    first_u64_at(value, &["/inputTokens", "/input_tokens"])
        .unwrap_or(0)
        .saturating_add(
            first_u64_at(value, &["/cachedInputTokens", "/cached_input_tokens"]).unwrap_or(0),
        )
        .saturating_add(first_u64_at(value, &["/outputTokens", "/output_tokens"]).unwrap_or(0))
        .saturating_add(
            first_u64_at(
                value,
                &["/reasoningOutputTokens", "/reasoning_output_tokens"],
            )
            .unwrap_or(0),
        )
}

pub(crate) fn codex_usage_token_count_kind(
    value: &serde_json::Value,
) -> Option<AgentContextTokenCountKind> {
    if let Some(last) = codex_usage_bucket(value, &["last", "last_token_usage"]) {
        let total = first_u64_at(last, &["/totalTokens", "/total_tokens"])?;
        return Some(if total > 0 && codex_usage_component_tokens(last) > 0 {
            AgentContextTokenCountKind::BackendReported
        } else {
            AgentContextTokenCountKind::LocalEstimate
        });
    }

    let total = first_u64_at(value, &["/totalTokens", "/total_tokens"])?;
    Some(if total > 0 && codex_usage_component_tokens(value) > 0 {
        AgentContextTokenCountKind::BackendReported
    } else {
        AgentContextTokenCountKind::Unknown
    })
}

pub(crate) fn codex_usage_context_window(value: &serde_json::Value) -> Option<u64> {
    first_u64_at(
        value,
        &[
            "/modelContextWindow",
            "/model_context_window",
            "/contextWindow",
            "/context_window",
            "/info/modelContextWindow",
            "/info/model_context_window",
            "/info/contextWindow",
            "/info/context_window",
        ],
    )
}

pub(crate) fn codex_usage_hard_context_window(value: &serde_json::Value) -> Option<u64> {
    let hard_context_window = first_u64_at(
        value,
        &[
            "/modelHardContextWindow",
            "/model_hard_context_window",
            "/hardContextWindow",
            "/hard_context_window",
            "/info/modelHardContextWindow",
            "/info/model_hard_context_window",
            "/info/hardContextWindow",
            "/info/hard_context_window",
        ],
    )?;
    let context_window = codex_usage_context_window(value);
    Some(normalize_codex_hard_context_window(
        context_window,
        hard_context_window,
    ))
}

pub(crate) fn normalize_codex_hard_context_window(
    context_window: Option<u64>,
    hard_context_window: u64,
) -> u64 {
    const GPT_5_4_CODEX_SOFT_CONTEXT_WINDOW: u64 = 258_400;
    const GPT_5_4_CODEX_HARD_CONTEXT_WINDOW: u64 = 272_000;

    if context_window == Some(GPT_5_4_CODEX_SOFT_CONTEXT_WINDOW)
        && hard_context_window <= GPT_5_4_CODEX_SOFT_CONTEXT_WINDOW
    {
        GPT_5_4_CODEX_HARD_CONTEXT_WINDOW
    } else {
        hard_context_window
    }
}

pub(crate) fn codex_usage_preserving_hard_context_window(
    mut usage: serde_json::Value,
    previous: Option<&serde_json::Value>,
) -> serde_json::Value {
    let Some(previous_hard) = previous.and_then(codex_usage_hard_context_window) else {
        return usage;
    };
    if previous_hard == 0 {
        return usage;
    }

    let context_window = codex_usage_context_window(&usage);
    let should_preserve = match codex_usage_hard_context_window(&usage) {
        Some(current_hard) if current_hard > 0 => context_window.is_some_and(|context_window| {
            current_hard <= context_window && previous_hard > current_hard
        }),
        _ => true,
    };
    if !should_preserve {
        return usage;
    }

    if let Some(object) = usage.as_object_mut() {
        object.insert(
            "model_hard_context_window".to_string(),
            serde_json::Value::from(previous_hard),
        );
    }
    usage
}

pub(crate) fn codex_context_pressure_floor_from_usage(
    usage: &serde_json::Value,
) -> Option<CodexContextPressureFloor> {
    if codex_usage_token_count_kind(usage) != Some(AgentContextTokenCountKind::BackendReported) {
        return None;
    }
    let token_count = codex_usage_total_tokens(usage)?;
    let context_window = codex_usage_context_window(usage)?;
    if context_window == 0 || token_count < context_window {
        return None;
    }
    Some(CodexContextPressureFloor {
        token_count,
        context_window,
        hard_context_window: codex_usage_hard_context_window(usage),
    })
}

pub(crate) fn codex_pressure_floor_applies(
    usage: Option<&serde_json::Value>,
    floor: CodexContextPressureFloor,
) -> bool {
    usage
        .and_then(codex_usage_context_window)
        .is_none_or(|context_window| context_window == floor.context_window)
}

pub(crate) fn codex_pressure_aware_usage_fields(
    usage: Option<&serde_json::Value>,
    pressure_floor: Option<CodexContextPressureFloor>,
) -> (
    Option<u64>,
    Option<AgentContextTokenCountKind>,
    Option<u64>,
    Option<u64>,
) {
    let mut token_count = usage.and_then(codex_usage_total_tokens);
    let mut token_count_kind = usage.and_then(codex_usage_token_count_kind);
    let mut context_window = usage.and_then(codex_usage_context_window);
    let mut hard_context_window = usage.and_then(codex_usage_hard_context_window);

    let Some(floor) = pressure_floor else {
        return (
            token_count,
            token_count_kind,
            context_window,
            hard_context_window,
        );
    };
    if !codex_pressure_floor_applies(usage, floor) {
        return (
            token_count,
            token_count_kind,
            context_window,
            hard_context_window,
        );
    }

    if context_window.is_none() {
        context_window = Some(floor.context_window);
    }
    if hard_context_window.is_none() {
        hard_context_window = floor.hard_context_window;
    }
    let should_use_floor = match (token_count, token_count_kind) {
        (Some(tokens), Some(AgentContextTokenCountKind::BackendReported)) => {
            tokens < floor.token_count
        }
        _ => true,
    };
    if should_use_floor {
        token_count = Some(floor.token_count);
        token_count_kind = Some(AgentContextTokenCountKind::BackendReported);
    }

    (
        token_count,
        token_count_kind,
        context_window,
        hard_context_window,
    )
}

pub(crate) async fn update_codex_context_pressure_floor(
    context_pressure_floor: &Arc<Mutex<Option<CodexContextPressureFloor>>>,
    usage: &serde_json::Value,
) {
    let Some(new_floor) = codex_context_pressure_floor_from_usage(usage) else {
        return;
    };
    let mut floor = context_pressure_floor.lock().await;
    match *floor {
        Some(existing)
            if existing.context_window == new_floor.context_window
                && existing.token_count >= new_floor.token_count => {}
        _ => *floor = Some(new_floor),
    }
}

pub(crate) fn codex_response_cwd(value: &serde_json::Value) -> Option<&str> {
    value
        .pointer("/cwd")
        .or_else(|| value.pointer("/thread/cwd"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

pub(crate) fn codex_thread_settings_cwd(value: &serde_json::Value) -> Option<&str> {
    value
        .pointer("/threadSettings/cwd")
        .or_else(|| value.pointer("/thread_settings/cwd"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

pub(crate) fn codex_paths_match(requested: &Path, actual: &str) -> bool {
    codex_path_compare_key(requested) == codex_path_compare_key(Path::new(actual))
}

pub(crate) fn codex_path_compare_key(path: &Path) -> String {
    let normalized = codex_lexically_normalize_path(path);
    let mut key = normalized.to_string_lossy().replace('\\', "/");
    while key.len() > 1 && key.ends_with('/') && !key.ends_with(":/") {
        key.pop();
    }
    key
}

pub(crate) fn codex_lexically_normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

pub(crate) fn codex_usage_input_tokens(value: &serde_json::Value) -> Option<u64> {
    first_u64_at(value, &["/inputTokens", "/input_tokens"])
}

pub(crate) fn codex_usage_output_tokens(value: &serde_json::Value) -> Option<u64> {
    first_u64_at(value, &["/outputTokens", "/output_tokens"])
}

pub(crate) fn codex_usage_cached_tokens(value: &serde_json::Value) -> Option<u64> {
    first_u64_at(
        value,
        &[
            "/cachedInputTokens",
            "/cached_input_tokens",
            "/cachedTokens",
            "/cached_tokens",
        ],
    )
}

pub(crate) fn codex_usage_cache_write_tokens(value: &serde_json::Value) -> Option<u64> {
    first_u64_at(
        value,
        &[
            "/cacheWriteTokens",
            "/cache_write_tokens",
            "/cacheCreationTokens",
            "/cache_creation_tokens",
            "/inputTokensDetails/cacheWriteTokens",
            "/input_tokens_details/cache_write_tokens",
        ],
    )
}

pub(crate) fn codex_usage_snapshot(
    value: &serde_json::Value,
    model: &str,
) -> Option<AgentUsageSnapshot> {
    if codex_usage_token_count_kind(value) != Some(AgentContextTokenCountKind::BackendReported) {
        return None;
    }

    let total = codex_usage_bucket(value, &["total", "total_token_usage"]).unwrap_or(value);
    let last = codex_usage_bucket(value, &["last", "last_token_usage"]);

    let prompt_tokens = codex_usage_input_tokens(total)?;
    let completion_tokens = codex_usage_output_tokens(total).unwrap_or(0);
    let cached_tokens = codex_usage_cached_tokens(total).unwrap_or(0);
    let cache_creation_tokens = codex_usage_cache_write_tokens(total).unwrap_or(0);
    let total_tokens = first_u64_at(total, &["/totalTokens", "/total_tokens"])
        .unwrap_or_else(|| prompt_tokens + completion_tokens);
    let tokens_used = last
        .and_then(|u| first_u64_at(u, &["/totalTokens", "/total_tokens"]))
        .unwrap_or(total_tokens);
    let context_window = codex_usage_context_window(value).unwrap_or(0);
    let hard_context_window = codex_usage_hard_context_window(value);
    let usage_pct = if context_window > 0 {
        tokens_used as f64 / context_window as f64 * 100.0
    } else {
        0.0
    };

    // Latest-request cache sample from the `last` bucket, for the vitals
    // hit receipt. GPT-5.6+ may report cache writes as a separate billed
    // subset of input tokens.
    let last_cache_read_tokens = last.and_then(codex_usage_cached_tokens).unwrap_or(0);
    let last_cache_creation_tokens = last.and_then(codex_usage_cache_write_tokens).unwrap_or(0);
    let last_uncached_input_tokens = last
        .and_then(codex_usage_input_tokens)
        .unwrap_or(0)
        .saturating_sub(last_cache_read_tokens.saturating_add(last_cache_creation_tokens));

    Some(AgentUsageSnapshot {
        provider: "openai".to_string(),
        model: model.to_string(),
        tokens_used,
        context_window,
        hard_context_window,
        usage_pct,
        prompt_tokens,
        completion_tokens,
        cached_tokens,
        cache_creation_tokens,
        last_cache_read_tokens,
        last_cache_creation_tokens,
        last_uncached_input_tokens,
        cache_ttl_seconds: (last_cache_creation_tokens > 0).then_some(30 * 60),
        // Attached by the notification pump from its rate-limit state.
        limits: Vec::new(),
    })
}

pub(crate) fn codex_request_item_count(payload: &serde_json::Value) -> Option<usize> {
    payload
        .get("input")
        .and_then(|v| v.as_array())
        .map(Vec::len)
}

// ---------------------------------------------------------------------------
// Reader task
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_snapshot_not_ready_suppresses_empty_trace_poll() {
        let err = CallerError::ExternalAgent(
            "no Codex inference request payload found in /tmp/traces".to_string(),
        );
        assert!(codex_context_snapshot_not_ready(&err));

        let other = CallerError::ExternalAgent("read Codex request trace entry: boom".to_string());
        assert!(!codex_context_snapshot_not_ready(&other));
    }

    #[test]
    fn context_archive_summary_compacts_raw_payload_for_visualization() {
        let large = "x".repeat(8_000);
        let payload = serde_json::json!({
            "instructions": large,
            "input": [
                {"type": "message", "role": "user", "content": "please inspect context use"}
            ],
            "model": "gpt-test",
        });
        let compact =
            codex_context_archive_payload(payload.clone(), "req-1", 1, "openai.test", false);
        let compact_json = serde_json::to_string(&compact).unwrap();
        assert_eq!(
            compact.pointer("/_intendant_context/archive_mode"),
            Some(&serde_json::json!("summary"))
        );
        assert_eq!(
            compact.pointer("/_intendant_context/raw_archived"),
            Some(&serde_json::json!(false))
        );
        assert_eq!(
            compact.pointer("/_intendant_context/raw_bytes"),
            Some(&serde_json::json!(context_json_len(&payload)))
        );
        assert!(compact_json.len() < context_json_len(&payload));
        assert!(!compact_json.contains(&"x".repeat(1_000)));
        assert!(compact
            .get("summary_parts")
            .and_then(|v| v.as_array())
            .is_some_and(|parts| parts.len() >= 2));
    }

    #[test]
    fn context_archive_exact_preserves_raw_payload() {
        let payload = serde_json::json!({
            "instructions": "keep me exact",
            "input": [{"role": "user", "content": "hello"}],
        });
        let exact = codex_context_archive_payload(payload, "req-1", 1, "openai.test", true);
        assert_eq!(
            exact.pointer("/_intendant_context/archive_mode"),
            Some(&serde_json::json!("exact"))
        );
        assert_eq!(
            exact.get("instructions").and_then(|v| v.as_str()),
            Some("keep me exact")
        );
    }

    #[test]
    fn codex_request_item_count_counts_input_items() {
        let payload = serde_json::json!({
            "input": [
                {"role": "developer"},
                {"role": "user"},
                {"type": "function_call_output"}
            ]
        });
        assert_eq!(codex_request_item_count(&payload), Some(3));
    }

    #[tokio::test]
    async fn codex_request_trace_reads_latest_inference_request_payload() {
        let tmp = tempfile::tempdir().unwrap();
        let first = tmp.path().join("trace-a");
        let second = tmp.path().join("trace-b");
        let current = tmp.path().join("trace-current");
        std::fs::create_dir_all(first.join("payloads")).unwrap();
        std::fs::create_dir_all(second.join("payloads")).unwrap();
        std::fs::create_dir_all(current.join("payloads")).unwrap();

        std::fs::write(
            first.join("payloads/0.json"),
            serde_json::json!({"input": [{"role": "old"}]}).to_string(),
        )
        .unwrap();
        std::fs::write(
            first.join("trace.jsonl"),
            serde_json::json!({
                "type": "event_msg",
                "ts": 1,
                "payload": {
                    "type": "inference_started",
                    "inference_call_id": "inference:old",
                    "request_payload": {
                        "kind": {"type": "inference_request"},
                        "provider_name": "OpenAI",
                        "path": "payloads/0.json"
                    }
                }
            })
            .to_string(),
        )
        .unwrap();

        std::fs::write(
            second.join("payloads/1.json"),
            serde_json::json!({"input": [{"role": "developer"}, {"role": "user"}]}).to_string(),
        )
        .unwrap();
        std::fs::write(
            second.join("trace.jsonl"),
            serde_json::json!({
                "type": "event_msg",
                "ts": 2,
                "payload": {
                    "type": "inference_started",
                    "inference_call_id": "inference:middle",
                    "request_payload": {
                        "kind": {"type": "inference_request"},
                        "provider_name": "OpenAI",
                        "path": "payloads/1.json"
                    }
                }
            })
            .to_string(),
        )
        .unwrap();

        std::fs::write(
            current.join("payloads/2.json"),
            serde_json::json!({
                "input": [
                    {"role": "developer"},
                    {"role": "user"},
                    {"type": "function_call_output"}
                ]
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            current.join("trace.jsonl"),
            serde_json::json!({
                "schema_version": 1,
                "seq": 1,
                "wall_time_unix_ms": 3,
                "payload": {
                    "type": "inference_started",
                    "provider_name": "OpenAI",
                    "inference_call_id": "inference:current",
                    "request_payload": {
                        "raw_payload_id": "raw_payload:2",
                        "kind": {"type": "inference_request"},
                        "path": "payloads/2.json"
                    }
                }
            })
            .to_string(),
        )
        .unwrap();

        let snapshot = read_latest_codex_request_payload(tmp.path()).await.unwrap();
        assert_eq!(snapshot.format, "openai.responses.resolved_request.v1");
        assert_eq!(snapshot.request_index, 3);
        assert!(snapshot.request_id.starts_with("codex-request-"));
        assert_eq!(snapshot.payload["input"].as_array().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn codex_request_trace_resolves_openai_previous_response_chain() {
        let tmp = tempfile::tempdir().unwrap();
        let trace = tmp.path().join("trace-thread-abc");
        std::fs::create_dir_all(trace.join("payloads")).unwrap();

        std::fs::write(
            trace.join("payloads/request-1.json"),
            serde_json::json!({
                "type": "response.create",
                "model": "gpt-5.5",
                "input": [
                    {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "first user message"}]}
                ]
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            trace.join("payloads/response-1.json"),
            serde_json::json!({
                "response_id": "resp_1",
                "output_items": [
                    {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "first assistant reply"}]}
                ]
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            trace.join("payloads/request-2.json"),
            serde_json::json!({
                "type": "response.create",
                "model": "gpt-5.5",
                "previous_response_id": "resp_1",
                "input": [
                    {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "second user message"}]}
                ]
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            trace.join("trace.jsonl"),
            [
                serde_json::json!({
                    "schema_version": 1,
                    "seq": 1,
                    "wall_time_unix_ms": 1,
                    "payload": {
                        "type": "inference_started",
                        "provider_name": "OpenAI",
                        "thread_id": "thread-abc",
                        "inference_call_id": "inference:1",
                        "request_payload": {
                            "kind": {"type": "inference_request"},
                            "path": "payloads/request-1.json"
                        }
                    }
                })
                .to_string(),
                serde_json::json!({
                    "schema_version": 1,
                    "seq": 2,
                    "wall_time_unix_ms": 2,
                    "payload": {
                        "type": "inference_completed",
                        "inference_call_id": "inference:1",
                        "response_id": "resp_1",
                        "response_payload": {
                            "kind": {"type": "inference_response"},
                            "path": "payloads/response-1.json"
                        }
                    }
                })
                .to_string(),
                serde_json::json!({
                    "schema_version": 1,
                    "seq": 3,
                    "wall_time_unix_ms": 3,
                    "payload": {
                        "type": "inference_started",
                        "provider_name": "OpenAI",
                        "thread_id": "thread-abc",
                        "inference_call_id": "inference:2",
                        "request_payload": {
                            "kind": {"type": "inference_request"},
                            "path": "payloads/request-2.json"
                        }
                    }
                })
                .to_string(),
            ]
            .join("\n"),
        )
        .unwrap();

        let snapshot = read_latest_codex_context_payload(tmp.path(), Some("thread-abc"))
            .await
            .unwrap();
        assert_eq!(snapshot.format, "openai.responses.resolved_request.v1");
        assert_eq!(snapshot.request_index, 2);
        let input = snapshot.payload["input"].as_array().unwrap();
        assert_eq!(input.len(), 3);
        let rendered = serde_json::to_string(&snapshot.payload).unwrap();
        assert!(rendered.contains("first user message"));
        assert!(rendered.contains("first assistant reply"));
        assert!(rendered.contains("second user message"));
        assert_eq!(
            snapshot.payload["_intendant_context"]["latest_request_input_count"],
            serde_json::json!(1)
        );
        assert_eq!(
            snapshot.payload["_intendant_context"]["resolved_input_count"],
            serde_json::json!(3)
        );
        assert_eq!(
            snapshot.payload["_intendant_context"]["request_index"],
            serde_json::json!(2)
        );
    }

    #[tokio::test]
    async fn codex_request_trace_reads_all_context_payloads_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        let trace = tmp.path().join("trace-thread-abc");
        std::fs::create_dir_all(trace.join("payloads")).unwrap();

        for (idx, text) in [(1, "first"), (2, "second"), (3, "third")] {
            std::fs::write(
                trace.join(format!("payloads/request-{idx}.json")),
                serde_json::json!({
                    "type": "response.create",
                    "input": [{"role": "user", "content": text}]
                })
                .to_string(),
            )
            .unwrap();
        }

        std::fs::write(
            trace.join("trace.jsonl"),
            [
                (30, 3, "inference:3"),
                (10, 1, "inference:1"),
                (20, 2, "inference:2"),
            ]
            .into_iter()
            .map(|(ts, idx, call_id)| {
                serde_json::json!({
                    "schema_version": 1,
                    "wall_time_unix_ms": ts,
                    "payload": {
                        "type": "inference_started",
                        "provider_name": "OpenAI",
                        "thread_id": "thread-abc",
                        "inference_call_id": call_id,
                        "request_payload": {
                            "kind": {"type": "inference_request"},
                            "path": format!("payloads/request-{idx}.json")
                        }
                    }
                })
                .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n"),
        )
        .unwrap();

        let snapshots = read_codex_context_payloads(tmp.path(), Some("thread-abc"))
            .await
            .unwrap();
        let indexes: Vec<u64> = snapshots
            .iter()
            .map(|snapshot| snapshot.request_index)
            .collect();
        assert_eq!(indexes, vec![1, 2, 3]);
        let rendered: Vec<String> = snapshots
            .iter()
            .map(|snapshot| serde_json::to_string(&snapshot.payload).unwrap())
            .collect();
        assert!(rendered[0].contains("first"));
        assert!(rendered[1].contains("second"));
        assert!(rendered[2].contains("third"));
        assert!(snapshots
            .windows(2)
            .all(|pair| pair[0].request_id != pair[1].request_id));
    }

    #[test]
    fn codex_token_usage_helpers_accept_app_server_shape() {
        let usage = serde_json::json!({
            "total": {
                "inputTokens": 1000,
                "cachedInputTokens": 300,
                "cacheWriteTokens": 150,
                "outputTokens": 200,
                "totalTokens": 1200
            },
            "last": {"inputTokens": 100, "cachedInputTokens": 60, "cacheWriteTokens": 15, "outputTokens": 25, "totalTokens": 125},
            "modelContextWindow": 128000,
            "modelHardContextWindow": 272000
        });
        assert_eq!(codex_usage_total_tokens(&usage), Some(125));
        assert_eq!(
            codex_usage_token_count_kind(&usage),
            Some(AgentContextTokenCountKind::BackendReported)
        );
        assert_eq!(codex_usage_context_window(&usage), Some(128000));
        assert_eq!(codex_usage_hard_context_window(&usage), Some(272000));
        let snapshot = codex_usage_snapshot(&usage, "gpt-5.4").unwrap();
        assert_eq!(snapshot.provider, "openai");
        assert_eq!(snapshot.model, "gpt-5.4");
        assert_eq!(snapshot.tokens_used, 125);
        assert_eq!(snapshot.context_window, 128000);
        assert_eq!(snapshot.hard_context_window, Some(272000));
        assert_eq!(snapshot.prompt_tokens, 1000);
        assert_eq!(snapshot.completion_tokens, 200);
        assert_eq!(snapshot.cached_tokens, 300);
        assert_eq!(snapshot.cache_creation_tokens, 150);
        assert!((snapshot.usage_pct - (125.0 / 128000.0 * 100.0)).abs() < 1e-12);
        // Cache-vitals sample comes from the `last` bucket, including the
        // GPT-5.6 cache-write dimension.
        assert_eq!(snapshot.last_cache_read_tokens, 60);
        assert_eq!(snapshot.last_uncached_input_tokens, 25);
        assert_eq!(snapshot.last_cache_creation_tokens, 15);
        assert_eq!(snapshot.cache_ttl_seconds, Some(1800));
    }

    #[test]
    fn codex_usage_preserves_known_hard_context_window_when_new_usage_collapses_to_soft() {
        let previous = serde_json::json!({
            "last_token_usage": {
                "input_tokens": 194000,
                "output_tokens": 275,
                "total_tokens": 194275
            },
            "model_context_window": 258400,
            "model_hard_context_window": 272000
        });
        let collapsed = serde_json::json!({
            "last_token_usage": {
                "input_tokens": 258000,
                "output_tokens": 400,
                "total_tokens": 258400
            },
            "model_context_window": 258400,
            "model_hard_context_window": 258400
        });

        let merged = codex_usage_preserving_hard_context_window(collapsed, Some(&previous));
        assert_eq!(codex_usage_context_window(&merged), Some(258400));
        assert_eq!(codex_usage_hard_context_window(&merged), Some(272000));
    }

    #[test]
    fn codex_usage_infers_known_hard_context_window_from_collapsed_first_sample() {
        let usage = serde_json::json!({
            "total_token_usage": {
                "input_tokens": 258000,
                "output_tokens": 400,
                "total_tokens": 258400
            },
            "last_token_usage": {
                "input_tokens": 258000,
                "output_tokens": 400,
                "total_tokens": 258400
            },
            "model_context_window": 258400,
            "model_hard_context_window": 258400
        });

        assert_eq!(codex_usage_context_window(&usage), Some(258400));
        assert_eq!(codex_usage_hard_context_window(&usage), Some(272000));
        let snapshot = codex_usage_snapshot(&usage, "codex").unwrap();
        assert_eq!(snapshot.hard_context_window, Some(272000));
    }

    #[test]
    fn codex_pressure_floor_keeps_saturated_rollout_rewind_only_until_reset() {
        let saturated = serde_json::json!({
            "last_token_usage": {
                "input_tokens": 258000,
                "output_tokens": 400,
                "total_tokens": 258400
            },
            "model_context_window": 258400,
            "model_hard_context_window": 272000
        });
        let floor = codex_context_pressure_floor_from_usage(&saturated).unwrap();
        assert_eq!(
            floor,
            CodexContextPressureFloor {
                token_count: 258400,
                context_window: 258400,
                hard_context_window: Some(272000),
            }
        );

        let short_failed_call = serde_json::json!({
            "last_token_usage": {
                "input_tokens": 149700,
                "output_tokens": 8,
                "total_tokens": 149708
            },
            "model_context_window": 258400,
            "model_hard_context_window": 272000
        });
        let (tokens, kind, context_window, hard_context_window) =
            codex_pressure_aware_usage_fields(Some(&short_failed_call), Some(floor));
        assert_eq!(tokens, Some(258400));
        assert_eq!(kind, Some(AgentContextTokenCountKind::BackendReported));
        assert_eq!(context_window, Some(258400));
        assert_eq!(hard_context_window, Some(272000));

        let (tokens, kind, _, _) =
            codex_pressure_aware_usage_fields(Some(&short_failed_call), None);
        assert_eq!(tokens, Some(149708));
        assert_eq!(kind, Some(AgentContextTokenCountKind::BackendReported));
    }

    #[test]
    fn codex_usage_keeps_new_larger_hard_context_window() {
        let previous = serde_json::json!({
            "last_token_usage": {"total_tokens": 10},
            "model_context_window": 100,
            "model_hard_context_window": 120
        });
        let expanded = serde_json::json!({
            "last_token_usage": {"total_tokens": 10},
            "model_context_window": 100,
            "model_hard_context_window": 200
        });

        let merged = codex_usage_preserving_hard_context_window(expanded, Some(&previous));
        assert_eq!(codex_usage_hard_context_window(&merged), Some(200));
    }

    #[test]
    fn codex_usage_snapshot_ignores_local_estimates() {
        let usage = serde_json::json!({
            "last_token_usage": {
                "total_tokens": 314358
            },
            "model_context_window": 258400,
            "model_hard_context_window": 272000
        });

        assert_eq!(
            codex_usage_token_count_kind(&usage),
            Some(AgentContextTokenCountKind::LocalEstimate)
        );
        assert!(codex_usage_snapshot(&usage, "codex").is_none());
    }
}
