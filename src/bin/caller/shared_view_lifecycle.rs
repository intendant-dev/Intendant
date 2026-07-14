//! Shared-view focus-annotation lifecycle (CU-05,
//! `docs/cu-e2e-findings-2026-07-13.md`).
//!
//! A `focus_shared_view` region/note is content-bound guidance ("Watch the
//! input and button highlight."), but the browser overlay it paints is
//! display-scoped: it used to survive the annotated content, the display
//! grant, and even the session that drew it, until a full `shared hide`.
//! This module is the daemon-side tracker that ends those orphans: it folds
//! the low-volume `AppEvent::SharedView` stream (the same last-event-wins
//! semantics the dashboard applies) and, when a lifecycle boundary passes —
//! the annotated display's user grant is revoked, or the owning session
//! ends — hands the control plane a `focus_clear` event to broadcast.
//! `focus_clear` is also the explicit, idempotent clear verb
//! (`clear_shared_view_focus` MCP tool, `intendant ctl shared focus clear`,
//! native `shared_view` action `focus_clear`); hiding the shared view
//! clears implicitly, matching the dashboard.
//!
//! Emitted clears re-enter this tracker through the same event lane and
//! fold to a no-op, so every path is idempotent by construction.

use crate::event::AppEvent;

/// Where a lifecycle-driven clear came from; rendered in the dashboard
/// banner and the session log line for the emitted `focus_clear`.
const REASON_DISPLAY_REVOKED: &str = "display access revoked";
const REASON_SESSION_ENDED: &str = "owning session ended";

/// The one live focus annotation, as the dashboards will be rendering it.
/// The shared view carries at most one (every `SharedView` event replaces
/// the previous region/note wholesale in the browser state).
#[derive(Debug, Clone, PartialEq, Eq)]
struct AnnotationRecord {
    display_target: Option<String>,
    /// Concrete display the annotation is bound to. Modern emitters always
    /// resolve a concrete id; `None` (legacy events) is treated as "unknown
    /// display" and matches any revocation, failing toward clearing.
    display_id: Option<u32>,
    /// Session that drew the annotation. `None` for owner-surface calls
    /// (stdio MCP), which only hide/revoke/explicit-clear can end.
    session_id: Option<String>,
}

/// Daemon-side mirror of the dashboards' shared-view focus state, consumed
/// by the control plane's intent loop (single writer, emission order).
#[derive(Debug, Default)]
pub(crate) struct SharedViewAnnotations {
    current: Option<AnnotationRecord>,
}

impl SharedViewAnnotations {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Fold one `AppEvent::SharedView` into the tracked state, mirroring
    /// the dashboard: `hide` and `focus_clear` drop any annotation; every
    /// other action *replaces* the annotation with the event's own
    /// region/note content (or drops it when the event carries none).
    /// Non-`SharedView` events are ignored.
    pub(crate) fn observe(&mut self, event: &AppEvent) {
        let AppEvent::SharedView {
            session_id,
            action,
            display_target,
            display_id,
            region,
            note,
            ..
        } = event
        else {
            return;
        };
        match action.as_str() {
            "hide" | "focus_clear" => self.current = None,
            _ => {
                let has_annotation_content =
                    region.is_some() || note.as_deref().is_some_and(|n| !n.trim().is_empty());
                self.current = has_annotation_content.then(|| AnnotationRecord {
                    display_target: display_target.clone(),
                    display_id: *display_id,
                    session_id: session_id.clone(),
                });
            }
        }
    }

    /// The user revoked (or auto-expiry revoked) a display grant: if the
    /// live annotation is bound to that display, return the `focus_clear`
    /// to broadcast. Idempotent — the annotation is taken before returning.
    pub(crate) fn on_user_display_revoked(&mut self, display_id: u32) -> Option<AppEvent> {
        let matches_display = self
            .current
            .as_ref()
            .is_some_and(|a| a.display_id.is_none_or(|id| id == display_id));
        if !matches_display {
            return None;
        }
        self.take_as_focus_clear(REASON_DISPLAY_REVOKED)
    }

    /// A session ended: if it owns the live annotation, return the
    /// `focus_clear` to broadcast. Annotations without a session owner are
    /// left for hide/revoke/explicit clear.
    pub(crate) fn on_session_ended(&mut self, session_id: &str) -> Option<AppEvent> {
        let owned = self
            .current
            .as_ref()
            .is_some_and(|a| a.session_id.as_deref() == Some(session_id));
        if !owned {
            return None;
        }
        self.take_as_focus_clear(REASON_SESSION_ENDED)
    }

    fn take_as_focus_clear(&mut self, reason: &str) -> Option<AppEvent> {
        let record = self.current.take()?;
        Some(AppEvent::SharedView {
            session_id: record.session_id,
            action: "focus_clear".to_string(),
            display_target: record.display_target,
            display_id: record.display_id,
            reason: Some(reason.to_string()),
            region: None,
            note: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SharedViewRegion;

    fn shared_view_event(
        action: &str,
        display_id: Option<u32>,
        session_id: Option<&str>,
        region: Option<SharedViewRegion>,
        note: Option<&str>,
    ) -> AppEvent {
        AppEvent::SharedView {
            session_id: session_id.map(str::to_string),
            action: action.to_string(),
            display_target: display_id.map(|id| format!("display_{id}")),
            display_id,
            reason: None,
            region,
            note: note.map(str::to_string),
        }
    }

    fn region() -> SharedViewRegion {
        SharedViewRegion {
            x: 0.25,
            y: 0.25,
            width: 0.5,
            height: 0.25,
        }
    }

    fn focus_on(display_id: u32, session_id: &str) -> AppEvent {
        shared_view_event(
            "focus",
            Some(display_id),
            Some(session_id),
            Some(region()),
            Some("Watch the input and button highlight."),
        )
    }

    fn assert_focus_clear(event: &AppEvent, display_id: u32, session_id: &str, reason: &str) {
        match event {
            AppEvent::SharedView {
                session_id: sid,
                action,
                display_id: did,
                reason: r,
                region,
                note,
                ..
            } => {
                assert_eq!(action, "focus_clear");
                assert_eq!(*did, Some(display_id));
                assert_eq!(sid.as_deref(), Some(session_id));
                assert_eq!(r.as_deref(), Some(reason));
                assert!(region.is_none() && note.is_none());
            }
            other => panic!("expected SharedView focus_clear, got {other:?}"),
        }
    }

    #[test]
    fn display_revocation_clears_the_annotation_on_that_display() {
        let mut state = SharedViewAnnotations::new();
        state.observe(&focus_on(0, "cu-session"));

        let clear = state
            .on_user_display_revoked(0)
            .expect("revoking the annotated display clears its annotation");
        assert_focus_clear(&clear, 0, "cu-session", "display access revoked");

        // Idempotent: the annotation was taken; a second revoke is silent.
        assert!(state.on_user_display_revoked(0).is_none());
    }

    #[test]
    fn revoking_an_unrelated_display_leaves_the_annotation() {
        let mut state = SharedViewAnnotations::new();
        state.observe(&focus_on(99, "cu-session"));
        assert!(state.on_user_display_revoked(0).is_none());
        // Still live: the owning session's end reaps it.
        assert!(state.on_session_ended("cu-session").is_some());
    }

    #[test]
    fn legacy_annotation_without_display_id_fails_toward_clearing() {
        let mut state = SharedViewAnnotations::new();
        state.observe(&shared_view_event(
            "focus",
            None,
            Some("cu-session"),
            Some(region()),
            None,
        ));
        assert!(
            state.on_user_display_revoked(0).is_some(),
            "an unknown-display annotation must clear on any revocation"
        );
    }

    #[test]
    fn owning_session_end_clears_and_other_sessions_do_not() {
        let mut state = SharedViewAnnotations::new();
        state.observe(&focus_on(99, "cu-session"));

        assert!(state.on_session_ended("bystander").is_none());
        let clear = state
            .on_session_ended("cu-session")
            .expect("owning session end clears the annotation");
        assert_focus_clear(&clear, 99, "cu-session", "owning session ended");
        assert!(state.on_session_ended("cu-session").is_none());
    }

    #[test]
    fn sessionless_annotations_survive_session_ends() {
        // Owner-surface (stdio MCP) focus calls carry no session id; no
        // session's end may reap them.
        let mut state = SharedViewAnnotations::new();
        state.observe(&shared_view_event(
            "focus",
            Some(0),
            None,
            Some(region()),
            None,
        ));
        assert!(state.on_session_ended("any-session").is_none());
        assert!(state.on_user_display_revoked(0).is_some());
    }

    #[test]
    fn hide_and_focus_clear_drop_the_annotation() {
        for clearing_action in ["hide", "focus_clear"] {
            let mut state = SharedViewAnnotations::new();
            state.observe(&focus_on(0, "cu-session"));
            state.observe(&shared_view_event(clearing_action, None, None, None, None));
            assert!(
                state.on_user_display_revoked(0).is_none(),
                "{clearing_action} must drop the annotation before any lifecycle trigger"
            );
        }
    }

    #[test]
    fn later_events_replace_the_annotation_like_the_dashboard_does() {
        let mut state = SharedViewAnnotations::new();
        state.observe(&focus_on(0, "first"));
        // A later show without region/note wipes the overlay client-side;
        // the tracker mirrors that.
        state.observe(&shared_view_event(
            "show",
            Some(0),
            Some("second"),
            None,
            None,
        ));
        assert!(state.on_user_display_revoked(0).is_none());

        // A show WITH a focus region (show_shared_view focus_region) or a
        // bare note registers annotation content again.
        state.observe(&shared_view_event(
            "show",
            Some(99),
            Some("third"),
            None,
            Some("look here"),
        ));
        let clear = state
            .on_session_ended("third")
            .expect("note-only content clears");
        assert_focus_clear(&clear, 99, "third", "owning session ended");
    }

    #[test]
    fn non_shared_view_events_are_ignored() {
        let mut state = SharedViewAnnotations::new();
        state.observe(&focus_on(0, "cu-session"));
        state.observe(&AppEvent::UserDisplayRevoked {
            display_id: 0,
            note: None,
        });
        // observe() ignores foreign events; only the dedicated hook reaps.
        assert!(state.on_user_display_revoked(0).is_some());
    }
}
