//! Negotiated MCP-over-HTTP Tasks sessions.
//!
//! The daemon's historical `/mcp` endpoint is stateless. Tasks need exactly one
//! piece of connection state: the client capabilities negotiated at initialize
//! plus a task store that must not bleed across principals. We therefore mint a
//! standard `Mcp-Session-Id` only for clients that declare SEP-2663 Tasks.
//! Legacy clients remain stateless and never enter this registry.

use super::HttpAccessContext;
use crate::mcp::RemoteTasks;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

const HTTP_TASK_SESSION_CAP: usize = 128;
const HTTP_TASK_SESSION_IDLE_TTL: Duration = Duration::from_secs(3 * 60 * 60);
const HTTP_TASK_SESSION_ID_MAX_BYTES: usize = 128;

#[derive(Clone, Debug, PartialEq, Eq)]
struct HttpTaskOwner {
    principal_id: String,
    principal_kind: String,
    source: String,
    transport: String,
    grant_id: Option<String>,
    authn_kind: Option<String>,
    authn_binding: Option<String>,
    authn_origin: Option<String>,
    gate_session: Option<String>,
}

impl HttpTaskOwner {
    fn from_access(access: &HttpAccessContext, gate_session: Option<&str>) -> Self {
        let principal = &access.principal;
        Self {
            principal_id: principal.id.clone(),
            principal_kind: principal.kind.clone(),
            source: principal.source.clone(),
            transport: principal.transport.clone(),
            grant_id: principal.grant_id.clone(),
            authn_kind: principal.authn_kind.clone(),
            authn_binding: principal.authn_binding.clone(),
            authn_origin: principal.authn_origin.clone(),
            gate_session: gate_session.map(str::to_string),
        }
    }
}

struct HttpTaskSessionEntry {
    owner: HttpTaskOwner,
    tasks: Arc<RemoteTasks>,
    last_seen: Instant,
}

#[derive(Clone)]
pub(crate) struct HttpTaskSession {
    id: String,
    tasks: Arc<RemoteTasks>,
}

impl HttpTaskSession {
    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn tasks(&self) -> &RemoteTasks {
        &self.tasks
    }
}

struct HttpTaskRegistry {
    sessions: HashMap<String, HttpTaskSessionEntry>,
    cap: usize,
    idle_ttl: Duration,
}

impl HttpTaskRegistry {
    fn new(cap: usize, idle_ttl: Duration) -> Self {
        Self {
            sessions: HashMap::new(),
            cap,
            idle_ttl,
        }
    }

    fn sweep(&mut self, now: Instant) {
        self.sessions.retain(|_, session| {
            let keep = now.saturating_duration_since(session.last_seen) <= self.idle_ttl;
            if !keep {
                session.tasks.request_shutdown();
            }
            keep
        });
    }

    fn create(&mut self, owner: HttpTaskOwner, now: Instant) -> Result<HttpTaskSession, String> {
        self.sweep(now);
        if self.sessions.len() >= self.cap {
            return Err(format!(
                "HTTP MCP Tasks session capacity reached ({})",
                self.cap
            ));
        }
        let id = loop {
            let id = uuid::Uuid::new_v4().simple().to_string();
            if !self.sessions.contains_key(&id) {
                break id;
            }
        };
        let tasks = Arc::new(RemoteTasks::new());
        self.sessions.insert(
            id.clone(),
            HttpTaskSessionEntry {
                owner,
                tasks: tasks.clone(),
                last_seen: now,
            },
        );
        Ok(HttpTaskSession { id, tasks })
    }

    fn resolve(
        &mut self,
        id: &str,
        owner: &HttpTaskOwner,
        now: Instant,
    ) -> Option<HttpTaskSession> {
        if id.is_empty() || id.len() > HTTP_TASK_SESSION_ID_MAX_BYTES {
            return None;
        }
        self.sweep(now);
        let session = self.sessions.get_mut(id)?;
        if &session.owner != owner {
            return None;
        }
        session.last_seen = now;
        Some(HttpTaskSession {
            id: id.to_string(),
            tasks: session.tasks.clone(),
        })
    }

    fn remove(&mut self, id: &str, owner: &HttpTaskOwner) -> Option<Arc<RemoteTasks>> {
        if id.is_empty() || id.len() > HTTP_TASK_SESSION_ID_MAX_BYTES {
            return None;
        }
        if self
            .sessions
            .get(id)
            .is_none_or(|session| &session.owner != owner)
        {
            return None;
        }
        self.sessions.remove(id).map(|session| session.tasks)
    }
}

static HTTP_TASK_SESSIONS: OnceLock<Mutex<HttpTaskRegistry>> = OnceLock::new();

fn http_task_sessions() -> &'static Mutex<HttpTaskRegistry> {
    HTTP_TASK_SESSIONS.get_or_init(|| {
        Mutex::new(HttpTaskRegistry::new(
            HTTP_TASK_SESSION_CAP,
            HTTP_TASK_SESSION_IDLE_TTL,
        ))
    })
}

pub(crate) fn create_http_task_session(
    access: &HttpAccessContext,
    gate_session: Option<&str>,
) -> Result<HttpTaskSession, String> {
    let owner = HttpTaskOwner::from_access(access, gate_session);
    http_task_sessions()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .create(owner, Instant::now())
}

pub(crate) fn resolve_http_task_session(
    id: &str,
    access: &HttpAccessContext,
    gate_session: Option<&str>,
) -> Option<HttpTaskSession> {
    let owner = HttpTaskOwner::from_access(access, gate_session);
    http_task_sessions()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .resolve(id, &owner, Instant::now())
}

pub(crate) async fn close_http_task_session(
    id: &str,
    access: &HttpAccessContext,
    gate_session: Option<&str>,
) -> bool {
    let owner = HttpTaskOwner::from_access(access, gate_session);
    let tasks = http_task_sessions()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(id, &owner);
    let Some(tasks) = tasks else {
        return false;
    };
    tasks.shutdown().await;
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn access(id: &str) -> HttpAccessContext {
        let mut principal = crate::access::iam::AccessPrincipal::local_loopback_mcp_default("http");
        principal.id = id.to_string();
        HttpAccessContext {
            principal,
            iam_state: None,
            peer_filesystem: None,
        }
    }

    #[test]
    fn session_ids_are_owner_bound() {
        let mut registry = HttpTaskRegistry::new(4, Duration::from_secs(60));
        let now = Instant::now();
        let owner_a = HttpTaskOwner::from_access(&access("principal:a"), Some("agent-a"));
        let owner_b = HttpTaskOwner::from_access(&access("principal:b"), Some("agent-b"));
        let session = registry.create(owner_a.clone(), now).unwrap();

        assert!(registry.resolve(session.id(), &owner_a, now).is_some());
        assert!(registry.resolve(session.id(), &owner_b, now).is_none());
        assert!(registry.remove(session.id(), &owner_b).is_none());
        let tasks = registry.remove(session.id(), &owner_a).unwrap();
        tasks.request_shutdown();
    }

    #[test]
    fn registry_is_bounded_and_idle_sessions_expire() {
        let mut registry = HttpTaskRegistry::new(1, Duration::from_millis(10));
        let now = Instant::now();
        let owner = HttpTaskOwner::from_access(&access("principal:a"), None);
        let first = registry.create(owner.clone(), now).unwrap();
        assert!(registry.create(owner.clone(), now).is_err());

        let later = now + Duration::from_millis(11);
        assert!(registry.resolve(first.id(), &owner, later).is_none());
        let second = registry.create(owner, later).unwrap();
        assert_ne!(first.id(), second.id());
        second.tasks().request_shutdown();
    }

    #[test]
    fn authorization_role_is_live_not_session_identity() {
        let mut root = access("principal:human");
        root.principal.grant_id = Some("grant:human".to_string());
        root.principal.authn_kind = Some("browser_mtls_cert".to_string());
        root.principal.authn_binding = Some("fingerprint".to_string());
        root.principal.role_id = "role:root".to_string();
        let owner = HttpTaskOwner::from_access(&root, None);

        let mut changed = root.clone();
        changed.principal.role_id = "role:viewer".to_string();
        assert_eq!(owner, HttpTaskOwner::from_access(&changed, None));
    }
}
