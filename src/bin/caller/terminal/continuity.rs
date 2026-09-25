//! Generation-bound MCP terminal references and graceful-update lifetime.
//!
//! A reference names one boot and one PTY, not just a reusable display name.
//! It conveys no authority: visibility and the tool's current IAM/scope checks
//! still apply. The tunnel relay routes it; this registry validates it again.

use super::*;
use base64::Engine as _;

pub(crate) const REFERENCE_PREFIX: &str = "iterm1.";
const MAX_TERMINAL_NAME_BYTES: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TerminalReference {
    pub(crate) boot_id: String,
    pub(crate) instance_id: String,
    pub(crate) name: String,
}

pub(crate) fn lost_reference() -> String {
    serde_json::json!({
        "ok": false,
        "code": "terminal_lost",
        "error": "the original terminal is unavailable; this handle will never open or target a replacement shell",
        "retry_safe": false,
        "hint": "verify the previous command's outcome before deliberately opening a new terminal",
    }).to_string()
}

pub(crate) fn invalid_reference() -> String {
    serde_json::json!({
        "ok": false,
        "code": "terminal_invalid_handle",
        "error": "invalid generation-bound terminal handle",
    })
    .to_string()
}

impl TerminalReference {
    pub(crate) fn parse(value: &str) -> Result<Option<Self>, String> {
        let Some(rest) = value.strip_prefix(REFERENCE_PREFIX) else {
            if value.len() > MAX_TERMINAL_NAME_BYTES {
                return Err(invalid_reference());
            }
            return Ok(None);
        };
        if rest.len() > 1600 {
            return Err(invalid_reference());
        }
        let fields: Vec<_> = rest.split('.').collect();
        if fields.len() != 3
            || !matches!(fields[0].len(), 26 | 32)
            || !fields[0]
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
            || fields[1].len() != 32
            || !fields[1]
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(invalid_reference());
        }
        let name = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(fields[2])
            .map_err(|_| invalid_reference())?;
        if name.is_empty() || name.len() > MAX_TERMINAL_NAME_BYTES {
            return Err(invalid_reference());
        }
        let name = String::from_utf8(name).map_err(|_| invalid_reference())?;
        Ok(Some(Self {
            boot_id: fields[0].to_owned(),
            instance_id: fields[1].to_owned(),
            name,
        }))
    }

    fn encode(boot_id: &str, instance_id: &str, name: &str) -> String {
        format!(
            "{REFERENCE_PREFIX}{boot_id}.{instance_id}.{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(name.as_bytes())
        )
    }
}

/// Separate from supervised-agent holdouts: these are not conversations and
/// must never enter session readoption/recovery. Exited shells retain their
/// final output until explicitly closed by their consumer.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct TerminalHoldout {
    pub(crate) terminal_id: String,
    pub(crate) state: String,
}

impl TerminalRegistry {
    pub(crate) fn bind_handover(&self, runtime: &Arc<crate::handover::HandoverRuntime>) {
        // Installed once, before the gateway admits requests. Weak links in
        // both directions avoid extending either owner's lifetime by a cycle.
        let _ = self.handover.set(Arc::downgrade(runtime));
    }

    fn owner_boot_id(&self) -> String {
        self.handover
            .get()
            .and_then(Weak::upgrade)
            .map(|runtime| runtime.boot_id().to_owned())
            .unwrap_or_else(|| self.fallback_boot_id.clone())
    }

    pub(crate) fn reference_for(&self, key: &TerminalKey, instance_id: &str) -> String {
        TerminalReference::encode(&self.owner_boot_id(), instance_id, &key.terminal_id)
    }

    pub(super) fn spawn_is_closed(&self) -> bool {
        self.spawn_closed.load(Ordering::Acquire)
            || self
                .handover
                .get()
                .and_then(Weak::upgrade)
                .is_some_and(|runtime| runtime.is_draining())
    }

    fn reference_key(&self, value: &str) -> Result<(TerminalKey, Option<String>), String> {
        match TerminalReference::parse(value)? {
            None => Ok((TerminalKey::local(value), None)),
            Some(reference) if reference.boot_id == self.owner_boot_id() => Ok((
                TerminalKey::local(reference.name),
                Some(reference.instance_id),
            )),
            Some(_) => Err(lost_reference()),
        }
    }

    pub(crate) async fn get_mcp_visible(
        &self,
        value: &str,
        actor: &TerminalActor,
    ) -> Result<Option<Arc<PtySession>>, String> {
        let (key, instance) = self.reference_key(value)?;
        let guard = self.sessions.read().await;
        if let Some(SessionSlot::Live(session)) = guard.get(&key) {
            if session.visible_to(actor)
                && instance
                    .as_ref()
                    .is_none_or(|id| id == &session.instance_id)
            {
                return Ok(Some(session.clone()));
            }
        }
        // Missing and invisible qualified references have identical answers.
        if instance.is_some() {
            Err(lost_reference())
        } else {
            Ok(None)
        }
    }

    pub(crate) async fn open_mcp(
        &self,
        value: &str,
        cols: u16,
        rows: u16,
        actor: &TerminalActor,
        policy: ShellSpawnPolicy,
    ) -> Result<(Arc<PtySession>, bool), String> {
        if TerminalReference::parse(value)?.is_some() {
            // Reopening a qualified reference is strictly attach-only, even
            // for an exited shell (its final output is still readable).
            return self
                .get_mcp_visible(value, actor)
                .await?
                .map(|session| (session, false))
                .ok_or_else(lost_reference);
        }
        self.open_or_attach(TerminalKey::local(value), cols, rows, actor, policy).await
            .map_err(|error| serde_json::json!({
                "ok": false,
                "code": if matches!(error, TerminalOpenError::Draining) { "daemon_draining" } else { "terminal_open_failed" },
                "error": error.to_string(),
            }).to_string())
    }

    pub(crate) async fn close_mcp_visible(
        &self,
        value: &str,
        actor: &TerminalActor,
    ) -> Result<bool, String> {
        let (key, instance) = self.reference_key(value)?;
        // Match and remove under ONE write lock: close must not validate an
        // old incarnation and then remove a concurrent replacement.
        let mut guard = self.sessions.write().await;
        let matches = matches!(guard.get(&key), Some(SessionSlot::Live(session))
            if session.visible_to(actor)
                && instance.as_ref().is_none_or(|id| id == &session.instance_id));
        if !matches {
            return if instance.is_some() {
                Err(lost_reference())
            } else {
                Ok(false)
            };
        }
        if let Some(SessionSlot::Live(session)) = guard.remove(&key) {
            session.write_input(&[0x04]);
        }
        Ok(true)
    }

    pub(crate) async fn freeze_for_drain(&self) -> Vec<TerminalHoldout> {
        let mut guard = self.sessions.write().await;
        // Same lock as Opening admission: after an empty snapshot no new
        // reservation can appear before the daemon's final exit check.
        self.spawn_closed.store(true, Ordering::Release);
        let abandoned: Vec<_> = guard
            .iter()
            .filter_map(|(key, slot)| match slot {
                SessionSlot::Opening(slot) if slot.done.has_changed().is_err() => Some(key.clone()),
                _ => None,
            })
            .collect();
        for key in abandoned {
            if let Some(SessionSlot::Opening(slot)) = guard.remove(&key) {
                if let Some(previous) = &slot.prev {
                    guard.insert(key, SessionSlot::Live(previous.clone()));
                }
            }
        }
        let mut rows: Vec<_> = guard
            .iter()
            .map(|(key, slot)| TerminalHoldout {
                terminal_id: key.terminal_id.clone(),
                state: match slot {
                    SessionSlot::Opening(_) => "opening",
                    SessionSlot::Live(session) if session.is_alive() => "running",
                    SessionSlot::Live(_) => "exited; close to release",
                }
                .to_owned(),
            })
            .collect();
        rows.sort_by(|a, b| a.terminal_id.cmp(&b.terminal_id));
        rows
    }
}

#[cfg(test)]
mod tests;
