//! Focus-panel drawing: the shared panel frame and row renderer,
//! scrollbar, composer strip, transcript panel, and the agent/view/host
//! focus layouts with their sliders.

use super::*;

impl StationInner {
    /// Shared focus-panel chrome: glass body, FOCUS kicker, title, and the
    /// close pill (with its hit zones). Body content is the caller's.
    pub(crate) fn focus_panel_frame(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        title: &str,
        color: &str,
    ) {
        self.glass_panel(
            x,
            y,
            w,
            h,
            10.0,
            hex_color(color).unwrap_or(C_IRIS),
            1.5,
            1.1,
        );
        self.hit_zones
            .push(HitZone::new(x, y, w, h, HitAction::Noop));
        self.text("FOCUS", x + 16.0, y + 23.0, 10.0, C_TEXT3_CSS, "bold");
        self.text(
            &truncate(title, 34),
            x + 16.0,
            y + 47.0,
            14.0,
            color,
            "bold",
        );
        self.pill_at(
            x + w - 70.0,
            y + 13.0,
            50.0,
            23.0,
            "close",
            C_TEXT3_CSS,
            false,
        );
        self.hit_zones.push(HitZone::new(
            x + w - 70.0,
            y + 13.0,
            50.0,
            23.0,
            HitAction::ClosePanel,
        ));
    }

    /// Scrollable, actionable rows panel: shared frame + header pills +
    /// uniform-height rows (click zones, per-row pills, choice pills) +
    /// scrollbar + a footer inspector line echoing the hovered row in
    /// full. The workhorse behind every system focus panel.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn rows_panel(
        &mut self,
        panel_id: &str,
        title: &str,
        color: &str,
        value: &str,
        detail: &str,
        surface: PanelSurface,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
    ) {
        self.focus_panel_frame(x, y, w, h, title, color);
        self.text(value, x + 16.0, y + 66.0, 10.5, C_TEXT_CSS, "normal");
        self.text(
            &truncate(detail, ((w - 30.0) / 5.8) as usize),
            x + 16.0,
            y + 84.0,
            9.0,
            C_TEXT2_CSS,
            "normal",
        );

        // Header pills: panel-wide operations, left to right.
        let mut px = x + 16.0;
        let py = y + 96.0;
        for pill in &surface.header {
            let pw = pill.label.chars().count() as f32 * 6.1 + 18.0;
            if px + pw > x + w - 16.0 {
                break;
            }
            self.pill_at(px, py, pw, 22.0, &pill.label, pill.color, pill.active);
            self.hit_zones
                .push(HitZone::new(px, py, pw, 22.0, pill.action.clone()));
            px += pw + 8.0;
        }

        // Scrollable rows viewport.
        let y0 = y + 128.0;
        let y1 = y + h - 26.0;
        let viewport_h = (y1 - y0).max(40.0);
        let rows = &surface.rows;
        let content_h = rows.len() as f32 * PANEL_ROW_H;
        self.scroll_zones.push(crate::input::ScrollZone {
            x,
            y: y0,
            w,
            h: viewport_h,
            panel: panel_id.to_string(),
            content_h,
        });
        let offset = self.scroll_offset(panel_id, content_h, viewport_h);
        let scrollable = content_h > viewport_h;
        let right_pad = if scrollable { 26.0 } else { 18.0 };

        if rows.is_empty() {
            if !surface.empty.is_empty() {
                self.text(
                    surface.empty,
                    x + 16.0,
                    y0 + 22.0,
                    10.0,
                    C_TEXT2_CSS,
                    "normal",
                );
            }
            return;
        }

        let ctx = self.hud.ctx.clone();
        ctx.save();
        self.rounded_path(x + 2.0, y0, w - 4.0, viewport_h, 6.0);
        ctx.clip();

        let first = (offset / PANEL_ROW_H).floor().max(0.0) as usize;
        let mut hovered_row: Option<usize> = None;
        for (idx, row) in rows.iter().enumerate().skip(first) {
            let ry = y0 + idx as f32 * PANEL_ROW_H - offset;
            if ry > y1 {
                break;
            }
            let hovered = self.hover_xy.is_some_and(|(hx, hy)| {
                hx >= x && hx <= x + w && hy >= ry && hy <= ry + PANEL_ROW_H && hy >= y0 && hy <= y1
            });
            if hovered {
                hovered_row = Some(idx);
                self.rounded_path(
                    x + 8.0,
                    ry + 1.0,
                    w - 8.0 - right_pad + 4.0,
                    PANEL_ROW_H - 3.0,
                    6.0,
                );
                self.hud.set_fill("rgba(126,140,250,0.10)");
                ctx.fill();
            }
            // Label column.
            self.text(
                &truncate(&row.label, 17),
                x + 16.0,
                ry + 19.0,
                9.0,
                row.color,
                "bold",
            );
            // Right-aligned pills; the value text yields to them.
            let mut pill_x = x + w - right_pad;
            for pill in row.pills.iter().rev() {
                let pw = pill.label.chars().count() as f32 * 5.6 + 14.0;
                pill_x -= pw;
                if pill_x < x + 130.0 {
                    break;
                }
                self.pill_at(pill_x, ry + 4.5, pw, 21.0, &pill.label, pill.color, false);
                self.hit_zones.push(HitZone::new(
                    pill_x,
                    ry + 4.5,
                    pw,
                    21.0,
                    pill.action.clone(),
                ));
                pill_x -= 6.0;
            }
            if row.choices.is_empty() {
                let max_chars = (((pill_x - 6.0) - (x + 124.0)) / 5.7).max(6.0) as usize;
                self.text(
                    &truncate(&row.value, max_chars),
                    x + 124.0,
                    ry + 19.0,
                    9.5,
                    C_TEXT_CSS,
                    "normal",
                );
            } else {
                // Choice pills row (autonomy / backend / toggles).
                let mut cx = x + 124.0;
                for (label, selected, action) in &row.choices {
                    let cw = label.chars().count() as f32 * 5.8 + 16.0;
                    if cx + cw > pill_x - 6.0 {
                        break;
                    }
                    self.pill_at(
                        cx,
                        ry + 4.5,
                        cw,
                        21.0,
                        label,
                        if *selected { row.color } else { C_TEXT3_CSS },
                        *selected,
                    );
                    self.hit_zones
                        .push(HitZone::new(cx, ry + 4.5, cw, 21.0, action.clone()));
                    cx += cw + 6.0;
                }
            }
            // Row body click zone (under the pills, which were pushed after
            // and therefore win hit-testing).
            if let Some(action) = &row.click {
                self.hit_zones.push(HitZone::new(
                    x + 8.0,
                    ry,
                    w - 8.0 - right_pad,
                    PANEL_ROW_H,
                    action.clone(),
                ));
            }
        }
        ctx.restore();
        self.hud.invalidate_styles();

        if scrollable {
            self.draw_scrollbar(x + w - 14.0, y0, viewport_h, content_h, offset);
        }

        // Footer inspector: the hovered row in full, since row values
        // truncate aggressively next to pills.
        if let Some(row) = hovered_row.and_then(|idx| rows.get(idx)) {
            self.text(
                &truncate(
                    &format!("{} — {}", row.label, row.value),
                    ((w - 28.0) / 4.9) as usize,
                ),
                x + 16.0,
                y + h - 9.0,
                8.5,
                C_TEXT2_CSS,
                "normal",
            );
        }
    }

    /// Slim scrollbar: rounded track + position thumb.
    pub(crate) fn draw_scrollbar(
        &self,
        x: f32,
        y: f32,
        viewport_h: f32,
        content_h: f32,
        offset: f32,
    ) {
        self.hud.set_fill("rgba(26,30,40,0.65)");
        self.rounded_path(x, y + 2.0, 6.0, viewport_h - 4.0, 3.0);
        self.hud.ctx.fill();
        let (thumb_h, thumb_off) =
            crate::input::scrollbar_thumb(viewport_h - 4.0, content_h, viewport_h, offset);
        self.hud.set_fill("rgba(126,140,250,0.55)");
        self.rounded_path(x, y + 2.0 + thumb_off, 6.0, thumb_h, 3.0);
        self.hud.ctx.fill();
    }

    /// Composer strip rect for the current mode: `(x, y, w, h)`. Shared
    /// between the strip painter and the transcript panel (which yields
    /// vertical space when both are open).
    pub(crate) fn composer_rect(&self, w: f32, h: f32) -> (f32, f32, f32, f32) {
        let lane_y = (h - lane_metrics(self.density, h).2 - 24.0).max(282.0);
        let strip_h = if self.composer_mode == "launch" {
            96.0
        } else {
            56.0
        };
        let sw = (w * 0.52).clamp(320.0, 660.0);
        (24.0, lane_y - strip_h - 12.0, sw, strip_h)
    }

    /// The composer strip: canvas-drawn chrome for the DOM input overlay.
    /// Send mode: target chip + input slot + send. Launch mode: input slot
    /// + agent choice pills + execution pills + launch.
    pub(crate) fn draw_composer_strip(&mut self, w: f32, h: f32) {
        let (x, y, sw, sh) = self.composer_rect(w, h);
        let controls = &self.snapshot.controls;
        let launch = self.composer_mode == "launch";
        self.glass_panel(x, y, sw, sh, 12.0, C_IRIS, 1.8, 1.08);
        self.hit_zones
            .push(HitZone::new(x, y, sw, sh, HitAction::Noop));

        let kicker = if launch {
            let missing = controls.launch_missing.trim();
            if controls.launch_ready || missing.is_empty() {
                "LAUNCH NEW SESSION".to_string()
            } else {
                format!("LAUNCH NEW SESSION — needs {}", truncate(missing, 28))
            }
        } else {
            format!("COMPOSE → {}", truncate(&self.station_target_label(), 36))
        };
        self.text(&kicker, x + 16.0, y + 16.0, 8.0, C_IRIS_CSS, "bold");
        self.text(
            "enter sends / esc closes",
            x + sw - 150.0,
            y + 16.0,
            7.5,
            C_TEXT3_CSS,
            "normal",
        );

        // Input slot: dark inset the DOM textarea sits over.
        let action_w = if launch { 88.0 } else { 76.0 };
        let slot_x = x + 14.0;
        let slot_w = sw - 28.0 - action_w - 10.0;
        let slot_y = y + 24.0;
        self.rounded_path(slot_x, slot_y, slot_w, 24.0, 7.0);
        self.hud.set_fill("rgba(9,10,18,0.78)");
        self.hud.ctx.fill();
        self.rounded_path(slot_x, slot_y, slot_w, 24.0, 7.0);
        self.hud.set_stroke("rgba(126,140,250,0.35)");
        self.hud.ctx.stroke();
        self.composer_input_rect = Some((slot_x + 6.0, slot_y + 2.0, slot_w - 12.0, 20.0));

        let send_x = slot_x + slot_w + 10.0;
        if launch {
            self.pill_at(
                send_x,
                slot_y + 1.0,
                action_w,
                22.0,
                "launch",
                C_SKY_CSS,
                true,
            );
            self.hit_zones.push(HitZone::new(
                send_x,
                slot_y + 1.0,
                action_w,
                22.0,
                HitAction::Composer { op: "launch" },
            ));
        } else {
            let label = if controls.prompt_mode == "steer" {
                "steer"
            } else {
                "send"
            };
            self.pill_at(
                send_x,
                slot_y + 1.0,
                action_w,
                22.0,
                label,
                C_IRIS_CSS,
                true,
            );
            self.hit_zones.push(HitZone::new(
                send_x,
                slot_y + 1.0,
                action_w,
                22.0,
                HitAction::Composer { op: "send" },
            ));
        }

        if launch {
            // Agent choice pills + execution shape pills. An empty
            // launch_mode means execution does not apply (external agent
            // selected); the agent pills then reclaim the full row.
            self.text("agent", x + 16.0, y + 70.0, 8.0, C_SKY_CSS, "bold");
            let execution = controls.launch_mode.as_str();
            let exec_pills = [
                ("auto", "auto", C_SKY_CSS),
                ("orch", "orchestrate", C_IRIS2_CSS),
                ("direct", "direct", C_AMBER_CSS),
            ];
            let exec_w = exec_pills
                .iter()
                .map(|(label, _, _)| label.chars().count() as f32 * 5.8 + 16.0 + 4.0)
                .sum::<f32>()
                - 4.0;
            let agent_limit = if execution.is_empty() {
                x + sw - 16.0
            } else {
                x + sw - exec_w - 24.0
            };
            let mut cx = x + 58.0;
            let selected_agent = controls.launch_agent.as_str();
            for (label, id) in [
                ("auto", ""),
                ("intendant", "internal"),
                ("codex", "codex"),
                ("claude", "claude-code"),
            ] {
                let cw = label.chars().count() as f32 * 5.8 + 16.0;
                if cx + cw > agent_limit {
                    break;
                }
                let active = selected_agent == id;
                self.pill_at(
                    cx,
                    y + 58.0,
                    cw,
                    21.0,
                    label,
                    if active { C_SKY_CSS } else { C_TEXT3_CSS },
                    active,
                );
                self.hit_zones.push(HitZone::new(
                    cx,
                    y + 58.0,
                    cw,
                    21.0,
                    HitAction::ControlsAction {
                        action: format!("launch-agent:{id}"),
                    },
                ));
                cx += cw + 6.0;
            }
            if !execution.is_empty() {
                let mut ex = x + sw - exec_w - 16.0;
                for (label, value, accent) in exec_pills {
                    let cw = label.chars().count() as f32 * 5.8 + 16.0;
                    let active = execution == value;
                    self.pill_at(
                        ex,
                        y + 58.0,
                        cw,
                        21.0,
                        label,
                        if active { accent } else { C_TEXT3_CSS },
                        active,
                    );
                    self.hit_zones.push(HitZone::new(
                        ex,
                        y + 58.0,
                        cw,
                        21.0,
                        HitAction::ControlsAction {
                            action: format!("launch-execution:{value}"),
                        },
                    ));
                    ex += cw + 4.0;
                }
            }
        } else {
            // Target chip: click opens the sessions panel to retarget.
            let chip_w = 70.0;
            self.pill_at(
                x + sw - 78.0 - action_w,
                y + 1.0,
                chip_w,
                18.0,
                "target",
                C_AMBER_CSS,
                false,
            );
            self.hit_zones.push(HitZone::new(
                x + sw - 78.0 - action_w,
                y + 1.0,
                chip_w,
                18.0,
                HitAction::Composer { op: "target" },
            ));
        }
    }

    /// Transcript / diff viewer: a large left-anchored panel with
    /// word-wrapped, kind-colored rows and pixel scrolling. Content
    /// layout (wrapping) is cached per (content, width) signature.
    pub(crate) fn draw_transcript_panel(&mut self, w: f32, h: f32) {
        let Some(transcript) = self.transcript.clone() else {
            return;
        };
        let lane_y = (h - lane_metrics(self.density, h).2 - 24.0).max(282.0);
        let x = 24.0;
        let tw = (w * 0.56).clamp(340.0, 820.0).min(w - 48.0);
        let top = 58.0 + 14.0;
        let bottom = if self.composer_open {
            self.composer_rect(w, h).1 - 10.0
        } else {
            lane_y - 12.0
        };
        let th = (bottom - top).max(180.0);
        let diff = transcript.mode == "diff";
        let accent = if diff { C_AMBER_CSS } else { C_SKY_CSS };

        self.glass_panel(
            x,
            top,
            tw,
            th,
            12.0,
            hex_color(accent).unwrap_or(C_SKY),
            1.4,
            1.06,
        );
        self.hit_zones
            .push(HitZone::new(x, top, tw, th, HitAction::Noop));
        self.text(
            if diff { "DIFF" } else { "TRANSCRIPT" },
            x + 16.0,
            top + 21.0,
            10.0,
            C_TEXT3_CSS,
            "bold",
        );
        self.text(
            &truncate(&nonempty(&transcript.label, &transcript.session_id), 42),
            x + 16.0,
            top + 43.0,
            13.0,
            accent,
            "bold",
        );
        self.pill_at(
            x + tw - 66.0,
            top + 12.0,
            50.0,
            22.0,
            "close",
            C_TEXT3_CSS,
            false,
        );
        self.hit_zones.push(HitZone::new(
            x + tw - 66.0,
            top + 12.0,
            50.0,
            22.0,
            HitAction::CloseTranscript,
        ));
        // Header ops.
        let mut hx = x + 16.0;
        let header_pills: Vec<(&str, &str, HitAction)> = if diff {
            vec![(
                "copy diff",
                C_IRIS_CSS,
                HitAction::ChangesAction {
                    action: "copy-diff".into(),
                    path: transcript.session_id.clone(),
                },
            )]
        } else {
            vec![
                ("steer", C_IRIS_CSS, HitAction::Composer { op: "open-send" }),
                (
                    "focus",
                    C_AMBER_CSS,
                    HitAction::SessionAction {
                        action: "focus".into(),
                        id: transcript.session_id.clone(),
                    },
                ),
                (
                    "copy id",
                    C_TEXT3_CSS,
                    HitAction::SessionAction {
                        action: "copy".into(),
                        id: transcript.session_id.clone(),
                    },
                ),
            ]
        };
        for (label, color, action) in header_pills {
            let pw = label.chars().count() as f32 * 6.1 + 18.0;
            self.pill_at(hx, top + 52.0, pw, 22.0, label, color, false);
            self.hit_zones
                .push(HitZone::new(hx, top + 52.0, pw, 22.0, action));
            hx += pw + 8.0;
        }
        self.text(
            &format!(
                "{} of {} entries",
                transcript.rows.len(),
                transcript.total.max(transcript.rows.len() as u32)
            ),
            x + tw - 190.0,
            top + 66.0,
            8.5,
            C_TEXT2_CSS,
            "normal",
        );

        let y0 = top + 84.0;
        let y1 = top + th - 14.0;
        let viewport_h = (y1 - y0).max(60.0);

        if !transcript.error.is_empty() {
            self.text(
                &truncate(&transcript.error, ((tw - 30.0) / 5.8) as usize),
                x + 16.0,
                y0 + 18.0,
                10.0,
                C_ROSE_CSS,
                "normal",
            );
            return;
        }
        if transcript.rows.is_empty() {
            self.text(
                "no entries — waiting for the session log",
                x + 16.0,
                y0 + 18.0,
                10.0,
                C_TEXT2_CSS,
                "normal",
            );
            return;
        }

        // Wrapped layout, cached against a cheap content signature.
        let gutter = if diff { 16.0 } else { 64.0 };
        let wrap_px = tw - gutter - 16.0 - 18.0;
        let wrap_chars = ((wrap_px / 5.6).max(20.0)) as usize;
        let sig = transcript_signature(&transcript, wrap_chars as u32);
        if self.transcript_layout.as_ref().map(|l| l.sig) != Some(sig) {
            let mut layout = layout_transcript(&transcript, wrap_chars);
            layout.sig = sig;
            self.transcript_layout = Some(layout);
        }
        let layout = self.transcript_layout.as_ref().unwrap();
        let content_h = layout.content_h;
        self.scroll_zones.push(crate::input::ScrollZone {
            x,
            y: y0,
            w: tw,
            h: viewport_h,
            panel: "transcript".to_string(),
            content_h,
        });
        if self.transcript_follow {
            let max = (content_h - viewport_h).max(0.0);
            self.panel_scroll.insert("transcript".to_string(), max);
        }
        let offset = self.scroll_offset("transcript", content_h, viewport_h);

        let ctx = self.hud.ctx.clone();
        ctx.save();
        self.rounded_path(x + 2.0, y0, tw - 4.0, viewport_h, 6.0);
        ctx.clip();
        let layout = self.transcript_layout.take().unwrap();
        for line in &layout.lines {
            let ly = y0 + line.y - offset;
            if ly < y0 - TRANSCRIPT_LINE_H {
                continue;
            }
            if ly > y1 + TRANSCRIPT_LINE_H {
                break;
            }
            let color = transcript_kind_color(&line.kind);
            if line.first && !diff {
                self.text(&truncate(&line.kind, 9), x + 16.0, ly, 8.0, color, "bold");
                if !line.ts.is_empty() {
                    self.text(
                        &truncate(&line.ts, 8),
                        x + 16.0,
                        ly + 9.0,
                        6.5,
                        C_TEXT3_CSS,
                        "normal",
                    );
                }
            }
            self.text(
                &line.text,
                x + gutter,
                ly,
                9.5,
                if diff {
                    color
                } else if line.kind == "user" {
                    C_TEXT_CSS
                } else {
                    color
                },
                if line.first && line.kind == "user" {
                    "bold"
                } else {
                    "normal"
                },
            );
        }
        self.transcript_layout = Some(layout);
        ctx.restore();
        self.hud.invalidate_styles();

        if content_h > viewport_h {
            self.draw_scrollbar(x + tw - 14.0, y0, viewport_h, content_h, offset);
        }
    }

    /// One labeled row inside a focus panel: colored label column, value
    /// text beside it. Returns the next row baseline.
    pub(crate) fn focus_row(
        &self,
        x: f32,
        row_y: f32,
        w: f32,
        label: &str,
        value: &str,
        color: &str,
    ) -> f32 {
        self.text(&truncate(label, 11), x + 16.0, row_y, 9.0, color, "bold");
        self.text(
            &truncate(value, ((w - 116.0) / 5.6).max(18.0) as usize),
            x + 96.0,
            row_y,
            9.5,
            C_TEXT_CSS,
            "normal",
        );
        row_y + 17.0
    }

    /// Real detail panel for a selected agent node: identity, model, phase,
    /// task, budget/usage, and — when an approval is pending — the approval
    /// command plus actionable approve/deny pills. Rows, pills, and the
    /// approval come from `focus_rows::agent_focus_content`, shared with
    /// the world pane (`scene::add_agent_focus_pane`) so the two surfaces
    /// cannot drift.
    pub(crate) fn draw_agent_focus(
        &mut self,
        agent: &crate::model::StationAgent,
        x: f32,
        panel_w: f32,
        activity_lane_y: f32,
    ) {
        let content = crate::focus_rows::agent_focus_content(
            agent,
            self.snapshot.hosts.first().map(|h| h.id.as_str()),
            epoch_seconds_now(),
        );
        let approval = content.approval.is_some();
        let is_session = !content.pills.is_empty();
        let rows = content.rows.len() + if approval { 2 } else { 0 };
        let panel_h = 74.0
            + rows as f32 * 17.0
            + if approval { 30.0 } else { 6.0 }
            + if is_session { 34.0 } else { 0.0 };
        let y = (activity_lane_y - panel_h - 12.0).max(58.0);
        let phase = phase_color_css(&agent.phase);
        self.focus_panel_frame(x, y, panel_w, panel_h, &agent.id, phase);
        self.text(
            &truncate(&content.subtitle, 30),
            x + 96.0,
            y + 23.0,
            9.0,
            C_TEXT2_CSS,
            "normal",
        );
        // Direct line to the worker: open the composer targeted at the
        // current prompt target (the dashboard resolves the routing).
        self.pill_at(
            x + panel_w - 132.0,
            y + 13.0,
            54.0,
            23.0,
            "steer",
            C_IRIS_CSS,
            false,
        );
        self.hit_zones.push(HitZone::new(
            x + panel_w - 132.0,
            y + 13.0,
            54.0,
            23.0,
            HitAction::Composer { op: "open-send" },
        ));

        let mut row_y = y + 70.0;
        for row in &content.rows {
            row_y = self.focus_row(x, row_y, panel_w, row.label, &row.value, row.color_css);
            if let Some(pct) = row.meter {
                self.meter(
                    x + 96.0,
                    row_y - 12.0,
                    panel_w - 116.0,
                    pct,
                    pressure_color(pct),
                );
                row_y += 6.0;
            }
        }

        if is_session {
            // Per-node action pills at session-window-kebab parity — the
            // shared content builds the set; every pill dispatches through
            // the dashboard's real session-action handler.
            let py = row_y - 2.0;
            let mut px = x + 96.0;
            for pill in &content.pills {
                let pw = pill.label.chars().count() as f32 * 6.1 + 18.0;
                if px + pw > x + panel_w - 16.0 {
                    break;
                }
                self.pill_at(px, py, pw, 23.0, pill.label, pill.color_css, false);
                self.hit_zones
                    .push(HitZone::new(px, py, pw, 23.0, pill.action.clone()));
                px += pw + 8.0;
            }
            row_y += 32.0;
        }

        if let Some(appr) = &content.approval {
            row_y = self.focus_row(
                x,
                row_y,
                panel_w,
                appr.row.label,
                &appr.row.value,
                appr.row.color_css,
            );
            let py = row_y - 6.0;
            self.pill_at(x + 96.0, py, 78.0, 23.0, "approve", C_GREEN_CSS, false);
            self.hit_zones.push(HitZone::new(
                x + 96.0,
                py,
                78.0,
                23.0,
                HitAction::Approval {
                    host_id: appr.host_id.clone(),
                    approval_id: appr.approval_id.clone(),
                    decision: "approve",
                },
            ));
            self.pill_at(x + 182.0, py, 58.0, 23.0, "deny", C_ROSE_CSS, false);
            self.hit_zones.push(HitZone::new(
                x + 182.0,
                py,
                58.0,
                23.0,
                HitAction::Approval {
                    host_id: appr.host_id.clone(),
                    approval_id: appr.approval_id.clone(),
                    decision: "deny",
                },
            ));
        }
    }

    /// View-settings panel for the system:view node: mood toggle pills plus
    /// drag-aware fov/motion/AR/density sliders. Scrubs apply live in the
    /// renderer; the released value is emitted as a `view_set` action that
    /// the dashboard persists and re-applies through `set_visuals`.
    pub(crate) fn draw_view_focus(&mut self, x: f32, panel_w: f32, activity_lane_y: f32) {
        let panel_h = 74.0 + 30.0 + 4.0 * 26.0 + 12.0;
        let y = (activity_lane_y - panel_h - 12.0).max(58.0);
        self.focus_panel_frame(x, y, panel_w, panel_h, "View", C_IRIS2_CSS);
        self.text(
            &format!("{} layout", self.layout.label()),
            x + 96.0,
            y + 23.0,
            9.0,
            C_TEXT2_CSS,
            "normal",
        );

        let mut row_y = y + 72.0;
        self.text("mood", x + 16.0, row_y, 9.0, C_IRIS2_CSS, "bold");
        for (idx, mood) in [Mood::Cockpit, Mood::Calm].into_iter().enumerate() {
            let px = x + 96.0 + idx as f32 * 86.0;
            let label = mood.label();
            self.pill_at(
                px,
                row_y - 16.0,
                78.0,
                23.0,
                label,
                if self.mood == mood {
                    C_IRIS2_CSS
                } else {
                    C_TEXT3_CSS
                },
                self.mood == mood,
            );
            self.hit_zones.push(HitZone::new(
                px,
                row_y - 16.0,
                78.0,
                23.0,
                HitAction::ViewSet {
                    key: "mood",
                    value: label,
                },
            ));
        }
        row_y += 30.0;

        let sliders = [
            (
                ViewSliderKey::Fov,
                "fov",
                format!("{}°", self.fov_deg.round() as i32),
            ),
            (
                ViewSliderKey::Motion,
                "motion",
                format!("{:.1}x", self.motion),
            ),
            (
                ViewSliderKey::Ar,
                "ar tilt",
                format!("{}%", (self.ar_strength * 100.0).round() as i32),
            ),
            (
                ViewSliderKey::Density,
                "density",
                format!("{:.1}", self.density),
            ),
        ];
        for (key, label, value_label) in sliders {
            row_y = self.focus_slider(x, row_y, panel_w, key, label, &value_label);
        }
    }

    /// One slider row: label, scrubbable track with fill + knob, value
    /// readout. The hit zone is exactly the track rect (taller for touch),
    /// which is also the geometry pointer x maps through.
    pub(crate) fn focus_slider(
        &mut self,
        x: f32,
        row_y: f32,
        w: f32,
        key: ViewSliderKey,
        label: &str,
        value_label: &str,
    ) -> f32 {
        self.text(label, x + 16.0, row_y, 9.0, C_IRIS2_CSS, "bold");
        let track_x = x + 96.0;
        let track_w = w - 96.0 - 72.0;
        let t = key.t_of(self.view_slider_value(key));
        self.hud.set_fill("rgba(26,30,40,0.92)");
        self.hud
            .ctx
            .fill_rect(track_x as f64, (row_y - 7.0) as f64, track_w as f64, 4.0);
        self.hud.set_fill(C_IRIS2_CSS);
        self.hud.ctx.fill_rect(
            track_x as f64,
            (row_y - 7.0) as f64,
            (track_w * t) as f64,
            4.0,
        );
        self.hud.ctx.begin_path();
        let _ = self.hud.ctx.arc(
            (track_x + track_w * t) as f64,
            (row_y - 5.0) as f64,
            5.5,
            0.0,
            std::f64::consts::TAU,
        );
        self.hud.ctx.fill();
        self.hud.set_stroke("rgba(11,13,18,0.9)");
        self.hud.ctx.stroke();
        self.text(value_label, x + w - 62.0, row_y, 9.0, C_TEXT_CSS, "normal");
        self.hit_zones.push(HitZone::new(
            track_x,
            row_y - 16.0,
            track_w,
            22.0,
            HitAction::ViewSlider { key },
        ));
        row_y + 26.0
    }

    /// Real detail panel for a selected host node: platform, link state,
    /// load meters, and what is running / streaming on it.
    pub(crate) fn draw_host_focus(
        &mut self,
        host: &crate::model::StationHost,
        x: f32,
        panel_w: f32,
        activity_lane_y: f32,
    ) {
        let panel_h = 74.0 + 4.0 * 17.0 + 6.0;
        let y = (activity_lane_y - panel_h - 12.0).max(58.0);
        let color = if host.connected {
            C_AMBER_CSS
        } else {
            C_ROSE_CSS
        };
        self.focus_panel_frame(x, y, panel_w, panel_h, &host.name, color);
        self.text(
            if host.connected {
                "connected"
            } else {
                "offline"
            },
            x + 96.0,
            y + 23.0,
            9.0,
            if host.connected {
                C_GREEN_CSS
            } else {
                C_ROSE_CSS
            },
            "bold",
        );
        let mut row_y = y + 70.0;
        row_y = self.focus_row(
            x,
            row_y,
            panel_w,
            "platform",
            &format!(
                "{} / {}",
                nonempty(&host.platform, "unknown"),
                nonempty(&host.region, "local")
            ),
            C_IRIS_CSS,
        );
        // Metrics without a real reading render as "n/a" with an empty
        // meter track — never as a fabricated percentage.
        let metric_row = |metric: Option<f32>| match metric {
            Some(v) => {
                let pct = (v / 100.0).clamp(0.0, 1.0);
                (pct_label(pct), pressure_color(pct), pct)
            }
            None => ("n/a".to_string(), C_TEXT2_CSS, 0.0),
        };
        let (cpu_text, cpu_color, cpu_pct) = metric_row(host.cpu);
        row_y = self.focus_row(x, row_y, panel_w, "cpu", &cpu_text, cpu_color);
        self.meter(x + 156.0, row_y - 12.0, panel_w - 176.0, cpu_pct, cpu_color);
        let (mem_text, mem_color, mem_pct) = metric_row(host.mem);
        row_y = self.focus_row(x, row_y, panel_w, "memory", &mem_text, mem_color);
        self.meter(x + 156.0, row_y - 12.0, panel_w - 176.0, mem_pct, mem_color);
        let agents = self
            .snapshot
            .agents
            .iter()
            .filter(|a| a.host_id == host.id)
            .count();
        let waiting = self
            .snapshot
            .agents
            .iter()
            .filter(|a| a.host_id == host.id && a.needs_approval)
            .count();
        let streams = self
            .display_sources
            .values()
            .filter(|s| s.host_id == host.id)
            .count();
        self.focus_row(
            x,
            row_y,
            panel_w,
            "running",
            &format!(
                "{agents} agent{} / {streams} stream{}{}",
                if agents == 1 { "" } else { "s" },
                if streams == 1 { "" } else { "s" },
                if waiting > 0 {
                    format!(" / {waiting} awaiting approval")
                } else {
                    String::new()
                }
            ),
            C_SKY_CSS,
        );
    }
}
