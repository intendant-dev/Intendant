//! Focus-panel surface builders: one `PanelSurface` constructor per
//! station panel (system, sessions, worktrees, activity, context,
//! managed, changes, peers, controls).

use super::*;

impl StationInner {
    /// Actionable surface for a system target's focus panel: header pills
    /// (panel-wide operations) plus scrollable rows whose clicks and pills
    /// dispatch the dashboard's real handlers. This is where the
    /// snapshot's per-domain arrays become *operable* pixels.
    pub(crate) fn system_panel_surface(&self, id: &str) -> PanelSurface {
        match id {
            "system:context" => self.context_panel_surface(),
            "system:managed" => self.managed_panel_surface(),
            "system:sessions" => self.sessions_panel_surface(),
            "system:worktrees" => self.worktrees_panel_surface(),
            "system:changes" => self.changes_panel_surface(),
            "system:peers" => self.peers_panel_surface(),
            "system:activity" => self.activity_panel_surface(),
            "system:controls" => self.controls_panel_surface(),
            _ => PanelSurface::default(),
        }
    }

    pub(crate) fn sessions_panel_surface(&self) -> PanelSurface {
        let mut surface = PanelSurface {
            header: vec![
                HeaderPill::new("new", C_SKY_CSS, HitAction::Composer { op: "open-launch" }),
                HeaderPill::new(
                    "refresh",
                    C_IRIS_CSS,
                    HitAction::SessionAction {
                        action: "refresh".into(),
                        id: String::new(),
                    },
                ),
                HeaderPill::new(
                    "worktrees",
                    C_TEXT3_CSS,
                    HitAction::Select("system:worktrees".into()),
                ),
            ],
            empty: "no sessions yet — new launches one",
            ..Default::default()
        };
        for target in self.snapshot.sessions.external_targets.iter() {
            let sid = nonempty(&target.session_id, &target.id);
            let mut row = PanelRow::new(
                truncate(&nonempty(&target.label, "external"), 16),
                format!("{} / {}", target.value, target.detail),
                tone_color_css(&target.tone),
            )
            .click(HitAction::SessionAction {
                action: "station-log".into(),
                id: sid.clone(),
            });
            if target.can_focus {
                row = row.pill(
                    "focus",
                    C_AMBER_CSS,
                    HitAction::SessionAction {
                        action: "focus".into(),
                        id: sid.clone(),
                    },
                );
            }
            if target.can_attach {
                row = row.pill(
                    "attach",
                    C_SKY_CSS,
                    HitAction::SessionAction {
                        action: "attach".into(),
                        id: sid.clone(),
                    },
                );
            }
            if target.can_stop {
                row = row.pill(
                    "stop",
                    C_ROSE_CSS,
                    HitAction::SessionAction {
                        action: "stop".into(),
                        id: sid.clone(),
                    },
                );
            }
            surface.rows.push(row);
        }
        for session in self.snapshot.sessions.recent.iter() {
            let sid = nonempty(&session.session_id, &session.id);
            let mut row = PanelRow::new(
                truncate(&session.value, 16),
                format!("{} / {}", session.label, session.detail),
                tone_color_css(&session.tone),
            )
            .click(HitAction::SessionAction {
                action: "station-log".into(),
                id: sid.clone(),
            });
            if session.can_focus {
                row = row.pill(
                    "focus",
                    C_AMBER_CSS,
                    HitAction::SessionAction {
                        action: "focus".into(),
                        id: sid.clone(),
                    },
                );
            }
            if session.can_resume {
                row = row.pill(
                    "resume",
                    C_GREEN_CSS,
                    HitAction::SessionAction {
                        action: "resume".into(),
                        id: sid.clone(),
                    },
                );
            }
            // `stop` ends the session gracefully AFTER the current turn;
            // `interrupt` aborts the turn itself. Mid-turn both intents
            // matter, so offer both pills when both capabilities exist.
            if session.can_interrupt {
                row = row.pill(
                    "halt",
                    C_AMBER_CSS,
                    HitAction::SessionAction {
                        action: "interrupt".into(),
                        id: sid.clone(),
                    },
                );
            }
            if session.can_stop {
                row = row.pill(
                    "stop",
                    C_ROSE_CSS,
                    HitAction::SessionAction {
                        action: "stop".into(),
                        id: sid.clone(),
                    },
                );
            } else if session.can_fork {
                // Codex threads: fork when no lifecycle pill needs the slot
                // (running sessions keep stop; /fork via the composer covers
                // them).
                row = row.pill(
                    "fork",
                    C_VIOLET_CSS,
                    HitAction::SessionAction {
                        action: "fork".into(),
                        id: sid.clone(),
                    },
                );
            }
            surface.rows.push(row);
        }
        surface
    }

    pub(crate) fn worktrees_panel_surface(&self) -> PanelSurface {
        let mut surface = PanelSurface {
            header: vec![
                HeaderPill::new(
                    "scan",
                    C_IRIS_CSS,
                    HitAction::SessionAction {
                        action: "worktrees-scan".into(),
                        id: String::new(),
                    },
                ),
                HeaderPill::new(
                    "sessions",
                    C_TEXT3_CSS,
                    HitAction::Select("system:sessions".into()),
                ),
            ],
            empty: "no worktrees scanned — scan discovers them",
            ..Default::default()
        };
        for worktree in self.snapshot.sessions.recent_worktrees.iter() {
            let path = nonempty(&worktree.id, &worktree.value);
            surface.rows.push(
                PanelRow::new(
                    truncate(&worktree.value, 16),
                    format!("{} / {}", worktree.label, worktree.detail),
                    tone_color_css(&worktree.tone),
                )
                .click(HitAction::SessionAction {
                    action: "worktree".into(),
                    id: path.clone(),
                })
                .pill(
                    "copy",
                    C_IRIS_CSS,
                    HitAction::SessionAction {
                        action: "worktree-copy".into(),
                        id: path,
                    },
                ),
            );
        }
        surface
    }

    pub(crate) fn activity_panel_surface(&self) -> PanelSurface {
        let activity = &self.snapshot.activity;
        let level = nonempty(&activity.level_filter, "all");
        let next_level = match activity.level_filter.as_str() {
            "" => "error",
            "error" => "warn",
            "warn" => "info",
            _ => "",
        };
        let mut surface = PanelSurface {
            header: vec![
                HeaderPill::new_owned(
                    format!("lvl {}", truncate(&level, 6)),
                    C_AMBER_CSS,
                    HitAction::ActivityAction {
                        action: format!("level:{next_level}"),
                        id: String::new(),
                    },
                ),
                HeaderPill::new(
                    "copy",
                    C_IRIS_CSS,
                    HitAction::ActivityAction {
                        action: "copy-visible".into(),
                        id: String::new(),
                    },
                ),
                HeaderPill::new(
                    "clear",
                    C_ROSE_CSS,
                    HitAction::ActivityAction {
                        action: "clear-log".into(),
                        id: String::new(),
                    },
                ),
            ],
            empty: "no retained activity yet",
            ..Default::default()
        };
        for event in self.snapshot.events.iter().rev() {
            let color = level_color_css(&event.level);
            let mut row = PanelRow::new(
                format!(
                    "{} {}",
                    truncate(&nonempty(&event.ts, "--"), 8),
                    truncate(&event.level, 5)
                ),
                truncate(&event.msg, 200),
                color,
            )
            .pill(
                "copy",
                C_IRIS_CSS,
                HitAction::ActivityAction {
                    action: "copy-event".into(),
                    id: event.id.clone(),
                },
            );
            if !event.session_id.is_empty() {
                row = row
                    .click(HitAction::SessionAction {
                        action: "station-log".into(),
                        id: event.session_id.clone(),
                    })
                    .pill(
                        "log",
                        C_SKY_CSS,
                        HitAction::SessionAction {
                            action: "station-log".into(),
                            id: event.session_id.clone(),
                        },
                    );
            }
            surface.rows.push(row);
        }
        surface
    }

    pub(crate) fn context_panel_surface(&self) -> PanelSurface {
        let context = &self.snapshot.context;
        let mut surface = PanelSurface {
            header: vec![
                HeaderPill::new(
                    "live",
                    C_GREEN_CSS,
                    HitAction::ContextAction {
                        action: "live".into(),
                        id: String::new(),
                    },
                ),
                HeaderPill::new(
                    "replay",
                    C_IRIS_CSS,
                    HitAction::ContextAction {
                        action: "replay".into(),
                        id: String::new(),
                    },
                ),
                HeaderPill::new(
                    "prev",
                    C_TEXT3_CSS,
                    HitAction::ContextAction {
                        action: "replay-prev".into(),
                        id: String::new(),
                    },
                ),
                HeaderPill::new(
                    "next",
                    C_TEXT3_CSS,
                    HitAction::ContextAction {
                        action: "replay-next".into(),
                        id: String::new(),
                    },
                ),
                HeaderPill::new(
                    "copy",
                    C_IRIS2_CSS,
                    HitAction::ContextAction {
                        action: "copy-snapshot".into(),
                        id: String::new(),
                    },
                ),
            ],
            empty: "no live context snapshot yet",
            ..Default::default()
        };
        if context.replay_count > 0 {
            surface.rows.push(PanelRow::new(
                "replay".to_string(),
                format!(
                    "{} / {} of {} / {}",
                    context.replay_mode,
                    context.replay_index,
                    context.replay_count,
                    nonempty(&context.replay_time, "-")
                ),
                C_IRIS_CSS,
            ));
        }
        for cat in context.top_categories.iter() {
            surface.rows.push(PanelRow::new(
                truncate(&cat.label, 16),
                format!(
                    "{} tok / {} / {}",
                    fmt_compact(cat.value),
                    cat.count,
                    cat.detail
                ),
                C_IRIS_CSS,
            ));
        }
        for item in context.top_items.iter() {
            surface.rows.push(
                PanelRow::new(
                    truncate(&item.label, 16),
                    format!("{} / {}", item.value, item.detail),
                    tone_color_css(&item.tone),
                )
                .pill(
                    "copy",
                    C_IRIS_CSS,
                    HitAction::ContextAction {
                        action: "copy-part".into(),
                        id: item.id.clone(),
                    },
                ),
            );
        }
        surface
    }

    pub(crate) fn managed_panel_surface(&self) -> PanelSurface {
        let managed = &self.snapshot.managed;
        let state = &managed.action_state;
        let mut header = vec![HeaderPill::new(
            "seed",
            C_IRIS_CSS,
            HitAction::ManagedAction {
                action: "seed-context".into(),
                id: String::new(),
            },
        )];
        if state.can_rewind {
            header.push(HeaderPill::new(
                "rewind",
                C_AMBER_CSS,
                HitAction::ManagedAction {
                    action: "dispatch-rewind".into(),
                    id: String::new(),
                },
            ));
        }
        if state.can_backout {
            header.push(HeaderPill::new(
                "backout",
                C_ROSE_CSS,
                HitAction::ManagedAction {
                    action: "run-backout".into(),
                    id: String::new(),
                },
            ));
        }
        header.push(HeaderPill::new(
            "status",
            C_TEXT3_CSS,
            HitAction::ManagedAction {
                action: "copy-status".into(),
                id: String::new(),
            },
        ));
        let mut surface = PanelSurface {
            header,
            empty: "no managed rewind records yet",
            ..Default::default()
        };
        if !state.readiness.trim().is_empty() || !state.result.trim().is_empty() {
            surface.rows.push(PanelRow::new(
                "state".to_string(),
                format!(
                    "{}{}",
                    state.readiness.trim(),
                    if state.result.trim().is_empty() {
                        String::new()
                    } else {
                        format!(" / {}", state.result.trim())
                    }
                ),
                C_IRIS2_CSS,
            ));
        }
        for record in managed.recent_records.iter() {
            let rid = nonempty(&record.id, &record.label);
            surface.rows.push(
                PanelRow::new(
                    truncate(&record.label, 16),
                    format!("{} / {}", record.value, record.detail),
                    tone_color_css(&record.tone),
                )
                .click(HitAction::ManagedAction {
                    action: "record-inspect".into(),
                    id: rid.clone(),
                })
                .pill(
                    "fork",
                    C_SKY_CSS,
                    HitAction::ManagedAction {
                        action: "record-fork".into(),
                        id: rid.clone(),
                    },
                )
                .pill(
                    "restore",
                    C_AMBER_CSS,
                    HitAction::ManagedAction {
                        action: "record-restore".into(),
                        id: rid,
                    },
                ),
            );
        }
        surface
    }

    pub(crate) fn changes_panel_surface(&self) -> PanelSurface {
        let mut surface = PanelSurface {
            header: vec![
                HeaderPill::new(
                    "refresh",
                    C_IRIS_CSS,
                    HitAction::ChangesAction {
                        action: "refresh".into(),
                        path: String::new(),
                    },
                ),
                HeaderPill::new(
                    "copy paths",
                    C_TEXT3_CSS,
                    HitAction::ChangesAction {
                        action: "copy-paths".into(),
                        path: String::new(),
                    },
                ),
            ],
            empty: "working tree clean",
            ..Default::default()
        };
        for change in self.snapshot.changes.recent.iter() {
            let path = nonempty(&change.id, &change.value);
            surface.rows.push(
                PanelRow::new(
                    truncate(&change.label, 16),
                    format!("{} / {}", change.value, change.detail),
                    tone_color_css(&change.tone),
                )
                .click(HitAction::ChangesAction {
                    action: "station-diff".into(),
                    path: path.clone(),
                })
                .pill(
                    "diff",
                    C_SKY_CSS,
                    HitAction::ChangesAction {
                        action: "station-diff".into(),
                        path: path.clone(),
                    },
                )
                .pill(
                    "copy",
                    C_IRIS_CSS,
                    HitAction::ChangesAction {
                        action: "copy-diff".into(),
                        path,
                    },
                ),
            );
        }
        surface
    }

    pub(crate) fn peers_panel_surface(&self) -> PanelSurface {
        let runway = &self.snapshot.display_runway;
        let mut surface = PanelSurface {
            header: vec![
                HeaderPill::new(
                    "refresh",
                    C_IRIS_CSS,
                    HitAction::ControlsAction {
                        action: "peer-refresh".into(),
                    },
                ),
                HeaderPill::new(
                    "open",
                    C_AMBER_CSS,
                    HitAction::ControlsAction {
                        action: "peer-open-selected".into(),
                    },
                ),
                HeaderPill::new(
                    "share",
                    C_GREEN_CSS,
                    HitAction::ControlsAction {
                        action: "display-toggle".into(),
                    },
                ),
            ],
            empty: "no peers or display lanes yet",
            ..Default::default()
        };
        if !runway.peer_status.trim().is_empty() {
            surface.rows.push(PanelRow::new(
                "status".to_string(),
                runway.peer_status.trim().to_string(),
                C_TEXT3_CSS,
            ));
        }
        for lane in runway.lanes.iter() {
            let tag = match lane.kind.as_str() {
                "local_stream" => "local",
                "remote_stream" => "remote",
                "peer_target" => "target",
                "operator_target" => "operator",
                "shared_view" => "shared",
                other => other,
            };
            let default_op = match lane.kind.as_str() {
                "peer_target" => "select",
                "operator_target" | "shared_view" => "focus",
                _ => "open",
            };
            let mut row = PanelRow::new(
                tag.to_string(),
                format!("{} / {} / {}", lane.title, lane.meta, lane.detail),
                if lane.selected {
                    C_IRIS_CSS
                } else {
                    C_AMBER_CSS
                },
            );
            if !lane.id.is_empty() {
                row = row
                    .click(HitAction::RunwayAction {
                        action: default_op.into(),
                        lane_id: lane.id.clone(),
                    })
                    .pill(
                        "open",
                        C_AMBER_CSS,
                        HitAction::RunwayAction {
                            action: "open".into(),
                            lane_id: lane.id.clone(),
                        },
                    )
                    .pill(
                        "copy",
                        C_IRIS_CSS,
                        HitAction::RunwayAction {
                            action: "copy".into(),
                            lane_id: lane.id.clone(),
                        },
                    );
                if !lane.session_id.is_empty() {
                    row = row.pill(
                        "log",
                        C_SKY_CSS,
                        HitAction::SessionAction {
                            action: "station-log".into(),
                            id: lane.session_id.clone(),
                        },
                    );
                }
            }
            surface.rows.push(row);
        }
        surface
    }

    pub(crate) fn controls_panel_surface(&self) -> PanelSurface {
        let controls = &self.snapshot.controls;
        let queue = &self.snapshot.attention_queue;
        let mut surface = PanelSurface {
            header: vec![
                HeaderPill::new(
                    "compose",
                    C_IRIS_CSS,
                    HitAction::Composer { op: "open-send" },
                ),
                HeaderPill::new(
                    "launch",
                    C_SKY_CSS,
                    HitAction::Composer { op: "open-launch" },
                ),
            ],
            empty: "",
            ..Default::default()
        };

        // Attention queue first: blocked work is what the operator came for.
        surface.rows.push(PanelRow::new(
            "attention".to_string(),
            format!(
                "{} blocked / {} warn / {} ready",
                queue.blocked, queue.warn, queue.ready
            ),
            if queue.blocked > 0 {
                C_ROSE_CSS
            } else if queue.warn > 0 {
                C_AMBER_CSS
            } else {
                C_GREEN_CSS
            },
        ));
        for item in queue.items.iter() {
            let mut row = PanelRow::new(
                truncate(&item.level, 16),
                format!("{} / {} / {}", item.title, item.meta, item.detail),
                attention_level_color_css(&item.level),
            );
            // `log:<session>` targets open that session's transcript;
            // anything else selects the named scene/system node.
            if let Some(session_id) = item.target.strip_prefix("log:") {
                row = row.click(HitAction::SessionAction {
                    action: "station-log".into(),
                    id: session_id.to_string(),
                });
            } else if !item.target.is_empty() {
                row = row.click(HitAction::Select(item.target.clone()));
            }
            surface.rows.push(row);
        }

        // Autonomy + backend selection as choice pill rows.
        surface.rows.push(PanelRow::choices(
            "autonomy",
            C_IRIS2_CSS,
            ["low", "medium", "high", "full"]
                .into_iter()
                .map(|level| {
                    (
                        level.to_string(),
                        controls.autonomy == level,
                        HitAction::ControlsAction {
                            action: format!("autonomy:{level}"),
                        },
                    )
                })
                .collect(),
        ));
        surface.rows.push(PanelRow::choices(
            "backend",
            C_VIOLET_CSS,
            [
                ("intendant", "internal"),
                ("codex", "codex"),
                ("claude", "claude-code"),
            ]
            .into_iter()
            .map(|(label, id)| {
                (
                    label.to_string(),
                    // The dashboard reports the no-external-agent state as
                    // "", "none", or "internal" depending on the source.
                    controls.backend == id
                        || (id == "internal"
                            && matches!(controls.backend.as_str(), "" | "none" | "internal")),
                    HitAction::ControlsAction {
                        action: format!("backend:{id}"),
                    },
                )
            })
            .collect(),
        ));
        // Codex runtime rows: visible whenever codex is the global backend
        // OR the launch composer is aimed at codex — configuring a codex
        // launch (approval/managed mode) must not require flipping the
        // global backend first.
        if controls.backend == "codex" || controls.launch_agent == "codex" {
            surface.rows.push(PanelRow::choices(
                "approval",
                C_AMBER_CSS,
                ["untrusted", "on-request", "never"]
                    .into_iter()
                    .map(|policy| {
                        (
                            policy.to_string(),
                            controls.approval_policy == policy,
                            HitAction::ControlsAction {
                                action: format!("codex-approval:{policy}"),
                            },
                        )
                    })
                    .collect(),
            ));
            surface.rows.push(PanelRow::choices(
                "managed ctx",
                C_VIOLET_CSS,
                ["vanilla", "managed"]
                    .into_iter()
                    .map(|mode| {
                        (
                            mode.to_string(),
                            controls.managed_context == mode
                                || (mode == "vanilla" && controls.managed_context.is_empty()),
                            HitAction::ControlsAction {
                                action: format!("codex-managed:{mode}"),
                            },
                        )
                    })
                    .collect(),
            ));
            // Managed mode only works with the Intendant-aware Codex
            // fork: show which binary managed sessions will spawn, and
            // flag the ambiguous legacy setup (managed selected, no
            // dedicated fork configured → falls back to `command`).
            if controls.managed_context == "managed" {
                if controls.managed_command.trim().is_empty() {
                    surface.rows.push(PanelRow::new(
                        "fork".to_string(),
                        format!(
                            "no managed fork set — using {} (must be the Intendant-aware fork)",
                            nonempty(&controls.command, "codex")
                        ),
                        C_AMBER_CSS,
                    ));
                } else {
                    surface.rows.push(PanelRow::new(
                        "fork".to_string(),
                        format!("managed sessions spawn {}", controls.managed_command.trim()),
                        C_GREEN_CSS,
                    ));
                }
            }
        }
        // Claude Code runtime rows: same visibility contract as the Codex
        // block — global backend OR the launch composer aimed at
        // claude-code. Model pills are the CLI's latest-version aliases
        // ("default" clears back to the CLI's own default); permission
        // pills use short labels for the CLI's camelCase modes.
        if controls.backend == "claude-code" || controls.launch_agent == "claude-code" {
            let model = controls.claude_model.trim();
            const MODEL_ALIASES: [&str; 4] = ["fable", "opus", "sonnet", "haiku"];
            surface.rows.push(PanelRow::choices(
                "model",
                C_AMBER_CSS,
                std::iter::once((
                    "default".to_string(),
                    model.is_empty(),
                    HitAction::ControlsAction {
                        action: "claude-model:default".into(),
                    },
                ))
                .chain(MODEL_ALIASES.into_iter().map(|alias| {
                    (
                        alias.to_string(),
                        model.contains(alias),
                        HitAction::ControlsAction {
                            action: format!("claude-model:{alias}"),
                        },
                    )
                }))
                .collect(),
            ));
            // A pinned model outside the alias set still shows truthfully.
            if !model.is_empty() && !MODEL_ALIASES.iter().any(|alias| model.contains(alias)) {
                surface.rows.push(PanelRow::new(
                    "model".to_string(),
                    format!("custom: {model}"),
                    C_SKY_CSS,
                ));
            }
            surface.rows.push(PanelRow::choices(
                "permissions",
                C_VIOLET_CSS,
                [
                    ("default", "default"),
                    ("edits", "acceptEdits"),
                    ("plan", "plan"),
                    ("auto", "auto"),
                    ("dontAsk", "dontAsk"),
                    ("bypass", "bypassPermissions"),
                ]
                .into_iter()
                .map(|(label, mode)| {
                    (
                        label.to_string(),
                        controls.claude_permission_mode == mode
                            || (mode == "default" && controls.claude_permission_mode.is_empty()),
                        HitAction::ControlsAction {
                            action: format!("claude-permission:{mode}"),
                        },
                    )
                })
                .collect(),
            ));
        }

        // Voice / video / display sharing toggles.
        surface.rows.push(PanelRow::choices(
            "av",
            C_SKY_CSS,
            vec![
                (
                    if controls.mic_active {
                        "mic on"
                    } else {
                        "mic off"
                    }
                    .to_string(),
                    controls.mic_active,
                    HitAction::ControlsAction {
                        action: "voice-toggle".into(),
                    },
                ),
                (
                    if controls.video_active {
                        "cam on"
                    } else {
                        "cam off"
                    }
                    .to_string(),
                    controls.video_active,
                    HitAction::ControlsAction {
                        action: "video-toggle".into(),
                    },
                ),
                (
                    "make active".to_string(),
                    controls.active_browser,
                    HitAction::ControlsAction {
                        action: "voice-active".into(),
                    },
                ),
            ],
        ));
        surface.rows.push(
            PanelRow::new(
                "display".to_string(),
                format!("share: {}", nonempty(&controls.display_access, "off")),
                C_AMBER_CSS,
            )
            .pill(
                "toggle",
                C_AMBER_CSS,
                HitAction::ControlsAction {
                    action: "display-toggle".into(),
                },
            )
            .pill(
                "list",
                C_IRIS_CSS,
                HitAction::ControlsAction {
                    action: "display-list".into(),
                },
            ),
        );

        // Browser workspaces.
        let mut browser_row = PanelRow::new(
            "browser".to_string(),
            format!(
                "{} workspace{} / {}",
                controls.browser_workspaces,
                if controls.browser_workspaces == 1 {
                    ""
                } else {
                    "s"
                },
                nonempty(&controls.browser_workspace_status, "idle")
            ),
            C_IRIS_CSS,
        );
        if controls.browser_workspace_can_create {
            browser_row = browser_row.pill(
                "create",
                C_GREEN_CSS,
                HitAction::ControlsAction {
                    action: "browser-create".into(),
                },
            );
        }
        if controls.browser_workspace_can_acquire {
            browser_row = browser_row.pill(
                "acquire",
                C_SKY_CSS,
                HitAction::ControlsAction {
                    action: "browser-acquire".into(),
                },
            );
        }
        if controls.browser_workspace_can_close {
            browser_row = browser_row.pill(
                "close",
                C_ROSE_CSS,
                HitAction::ControlsAction {
                    action: "browser-close".into(),
                },
            );
        }
        if !controls.browser_workspace_url.is_empty() {
            browser_row = browser_row.pill(
                "copy",
                C_IRIS_CSS,
                HitAction::ControlsAction {
                    action: "browser-copy".into(),
                },
            );
        }
        surface.rows.push(browser_row);

        // Recordings: live state + per-stream rows from the side cache.
        surface.rows.push(
            PanelRow::new(
                "recording".to_string(),
                if controls.active_recording.is_empty() {
                    format!("{} stored", controls.recordings)
                } else {
                    format!(
                        "recording {} / {} stored",
                        controls.active_recording, controls.recordings
                    )
                },
                if controls.debug_recording {
                    C_ROSE_CSS
                } else {
                    C_TEXT3_CSS
                },
            )
            .pill(
                if controls.debug_recording {
                    "stop rec"
                } else {
                    "record"
                },
                C_ROSE_CSS,
                HitAction::ControlsAction {
                    action: "debug-record".into(),
                },
            )
            .pill(
                "screen",
                C_IRIS_CSS,
                HitAction::ControlsAction {
                    action: "debug-screen".into(),
                },
            ),
        );
        for stream in controls.recording_streams.iter() {
            let name = nonempty(&stream.action_id, &stream.label);
            surface.rows.push(
                PanelRow::new(
                    truncate(&stream.label, 16),
                    format!("{} / {}", stream.value, stream.detail),
                    C_IRIS2_CSS,
                )
                .click(HitAction::ControlsAction {
                    action: format!("recording-open:{name}"),
                })
                .pill(
                    "play",
                    C_SKY_CSS,
                    HitAction::ControlsAction {
                        action: format!("recording-open:{name}"),
                    },
                ),
            );
        }

        // Computer use status.
        surface.rows.push(PanelRow::new(
            "computer".to_string(),
            format!(
                "{} / {} / {}",
                nonempty(&controls.cu_backend, "cu"),
                nonempty(&controls.cu_provider, "provider"),
                nonempty(&controls.cu_validation_state, "unvalidated")
            ),
            C_TEXT3_CSS,
        ));
        surface
    }
}
