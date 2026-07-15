//! HUD widget primitives: focus buttons, target labels, primary
//! actions, system-target layout, corner/compass chrome, meters,
//! pill buttons, rounded paths, glass panels, and the text/line/css
//! measurement helpers.

use super::*;

impl StationInner {
    pub(crate) fn station_focus_button(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        target: &SystemTarget,
    ) {
        let SystemTarget {
            id,
            kicker,
            title,
            color,
            ..
        } = *target;
        let value = &target.value;
        let detail = &target.detail;
        let selected = self.selected_id.as_deref() == Some(id);
        let hovered = self
            .hover_xy
            .is_some_and(|(hx, hy)| hx >= x && hx <= x + w && hy >= y && hy <= y + h);
        self.glass_panel(
            x,
            y,
            w,
            h,
            8.0,
            hex_color(color).unwrap_or(C_TEXT3),
            if selected {
                1.7
            } else if hovered {
                1.2
            } else {
                0.7
            },
            if selected { 1.1 } else { 0.85 },
        );
        self.hud.set_fill(color);
        self.hud
            .ctx
            .fill_rect((x + 9.0) as f64, (y + 10.0) as f64, 4.0, (h - 20.0) as f64);
        let max_chars = ((w - 34.0) / 6.2).max(12.0) as usize;
        if h < 38.0 {
            self.text(
                &truncate(title, max_chars),
                x + 20.0,
                y + h * 0.5 + 4.0,
                9.0,
                color,
                "bold",
            );
        } else if h < 58.0 {
            self.text(title, x + 20.0, y + 18.0, 10.0, color, "bold");
            self.text(
                &truncate(value, max_chars),
                x + 20.0,
                y + 35.0,
                9.5,
                C_TEXT_CSS,
                "normal",
            );
        } else if h < 72.0 {
            if !kicker.is_empty() {
                self.text(kicker, x + 20.0, y + 15.0, 7.5, C_TEXT3_CSS, "bold");
            }
            self.text(
                title,
                x + 20.0,
                y + if kicker.is_empty() { 21.0 } else { 29.0 },
                10.5,
                color,
                "bold",
            );
            self.text(
                &truncate(value, max_chars),
                x + 20.0,
                y + if detail.is_empty() {
                    h - 13.0
                } else {
                    h - 25.0
                },
                9.5,
                C_TEXT_CSS,
                "normal",
            );
            if !detail.is_empty() {
                self.text(
                    &truncate(detail, max_chars),
                    x + 20.0,
                    y + h - 11.0,
                    8.0,
                    C_TEXT2_CSS,
                    "normal",
                );
            }
        } else {
            if !kicker.is_empty() {
                self.text(kicker, x + 20.0, y + 16.0, 8.0, C_TEXT3_CSS, "bold");
            }
            self.text(
                title,
                x + 20.0,
                y + if kicker.is_empty() { 24.0 } else { 34.0 },
                11.0,
                color,
                "bold",
            );
            self.text(
                &truncate(value, max_chars),
                x + 20.0,
                y + h - if detail.is_empty() { 15.0 } else { 29.0 },
                10.0,
                C_TEXT_CSS,
                "normal",
            );
            if !detail.is_empty() {
                self.text(
                    &truncate(detail, max_chars),
                    x + 20.0,
                    y + h - 12.0,
                    8.5,
                    C_TEXT2_CSS,
                    "normal",
                );
            }
        }
        self.hit_zones
            .push(HitZone::new(x, y, w, h, HitAction::Select(id.to_string())));
    }

    pub(crate) fn station_target_label(&self) -> String {
        let controls = &self.snapshot.controls;
        nonempty(
            &controls.session_label,
            &nonempty(
                &controls.session_selection,
                &nonempty(&controls.command, "No active command target"),
            ),
        )
    }

    pub(crate) fn station_primary_actions(&self) -> Vec<LaneAction> {
        let controls = &self.snapshot.controls;
        // send/new session open the in-canvas composer (send + launch
        // modes) — they used to focus inputs on hidden dashboard tabs.
        let mut actions = vec![
            LaneAction::composer(
                if controls.prompt_mode == "steer" {
                    "steer"
                } else {
                    "send"
                },
                "open-send",
                72.0,
                C_IRIS_CSS,
            ),
            LaneAction::composer("new session", "open-launch", 112.0, C_SKY_CSS),
        ];
        if controls.session_can_focus {
            actions.push(LaneAction::activity("focus", "target", 72.0, C_AMBER_CSS));
        }
        if controls.session_can_interrupt {
            actions.push(LaneAction::activity("stop", "stop", 60.0, C_ROSE_CSS));
        }
        if controls.shared_view_can_take_input {
            actions.push(LaneAction::controls(
                "take input",
                "shared-view-take-input",
                102.0,
                C_GREEN_CSS,
            ));
        }
        actions.extend([
            LaneAction::select("context", "system:context", 82.0, C_IRIS_CSS),
            LaneAction::select("managed", "system:managed", 88.0, C_VIOLET_CSS),
            LaneAction::select("sessions", "system:sessions", 90.0, C_SKY_CSS),
            LaneAction::select("controls", "system:controls", 88.0, C_VIOLET_CSS),
        ]);
        actions
    }

    pub(crate) fn compute_system_targets(&self) -> Vec<SystemTarget> {
        let latest_event = self.snapshot.events.last();
        let ctx_pct = percent(
            self.snapshot.context.tokens,
            self.snapshot.context.effective_window,
        );
        let managed_pct = percent(
            self.snapshot.managed.used_tokens,
            self.snapshot.managed.effective_window,
        );
        let changes = &self.snapshot.changes;
        let controls = &self.snapshot.controls;
        let peer_count = self.snapshot.hosts.len().saturating_sub(1);
        vec![
            SystemTarget {
                id: "system:activity",
                kicker: "signal",
                title: "Activity",
                value: format!("{} retained", activity_retained_count(&self.snapshot)),
                detail: latest_event
                    .map(|event| truncate(&format!("{} {}", event.level, event.msg), 30))
                    .unwrap_or_else(|| "waiting for events".to_string()),
                color: latest_event
                    .map(|event| level_color_css(&event.level))
                    .unwrap_or(C_SKY_CSS),
            },
            SystemTarget {
                id: "system:context",
                kicker: "memory",
                title: "Context",
                value: if self.snapshot.context.available {
                    format!(
                        "{} / {} items",
                        pct_label(ctx_pct),
                        self.snapshot.context.item_count
                    )
                } else {
                    "waiting".to_string()
                },
                detail: truncate(
                    &format!(
                        "{} {}",
                        nonempty(&self.snapshot.context.source, "snapshot"),
                        nonempty(&self.snapshot.context.turn, "")
                    ),
                    30,
                ),
                color: pressure_color(ctx_pct),
            },
            SystemTarget {
                id: "system:managed",
                kicker: "lineage",
                title: "Managed",
                value: format!(
                    "{} / {}",
                    nonempty(&self.snapshot.managed.mode, "managed"),
                    nonempty(&self.snapshot.managed.status, "unknown")
                ),
                detail: format!(
                    "{} records / {} anchors",
                    self.snapshot.managed.records, self.snapshot.managed.anchors
                ),
                color: pressure_color(managed_pct),
            },
            SystemTarget {
                id: "system:controls",
                kicker: "operator",
                title: "Controls",
                value: truncate(
                    &format!(
                        "{} / {}",
                        nonempty(&controls.backend, "agent"),
                        nonempty(&controls.sandbox, "sandbox")
                    ),
                    32,
                ),
                detail: truncate(
                    &format!(
                        "{} / managed {}",
                        nonempty(&controls.approval_policy, "approval"),
                        nonempty(&controls.managed_context, "unknown")
                    ),
                    34,
                ),
                color: C_VIOLET_CSS,
            },
            SystemTarget {
                id: "system:sessions",
                kicker: "work",
                title: "Sessions",
                value: format!(
                    "{} total / {} active",
                    self.snapshot.sessions.total, self.snapshot.sessions.active
                ),
                detail: truncate(
                    &nonempty(&self.snapshot.sessions.latest_task, "launch history"),
                    32,
                ),
                color: if self.snapshot.sessions.active > 0 {
                    C_SKY_CSS
                } else {
                    C_IRIS_CSS
                },
            },
            SystemTarget {
                id: "system:peers",
                kicker: "display",
                title: "Peers",
                value: format!(
                    "{peer_count} peers / {} streams",
                    self.display_sources.len()
                ),
                detail: truncate(
                    &format!(
                        "{} / {}",
                        nonempty(&controls.display_access, "display"),
                        nonempty(&controls.cu_backend, "computer use")
                    ),
                    34,
                ),
                color: C_AMBER_CSS,
            },
            SystemTarget {
                id: "system:changes",
                kicker: "tree",
                title: "Changes",
                value: if changes.count > 0 {
                    format!(
                        "{} files / +{} -{}",
                        changes.count, changes.total_added, changes.total_removed
                    )
                } else {
                    nonempty(&changes.status, "clean")
                },
                detail: truncate(&nonempty(&changes.latest_path, "working tree clean"), 34),
                color: if changes.count > 0 || changes.status == "mismatch" {
                    C_AMBER_CSS
                } else {
                    C_GREEN_CSS
                },
            },
            SystemTarget {
                id: "system:worktrees",
                kicker: "project",
                title: "Worktrees",
                value: format!(
                    "{} scanned / {} active",
                    self.snapshot.sessions.worktrees, self.snapshot.sessions.worktree_active
                ),
                detail: format!(
                    "{} dirty / {} unmerged",
                    self.snapshot.sessions.worktree_dirty, self.snapshot.sessions.worktree_unmerged
                ),
                color: if self.snapshot.sessions.worktree_dirty > 0
                    || self.snapshot.sessions.worktree_unmerged > 0
                {
                    C_AMBER_CSS
                } else {
                    C_IRIS_CSS
                },
            },
            SystemTarget {
                id: "system:view",
                kicker: "scene",
                title: "View",
                value: format!("{} / {}", self.layout.label(), self.mood.label()),
                detail: format!(
                    "{} fov / {:.1} density",
                    self.fov_deg.round() as i32,
                    self.density
                ),
                color: C_IRIS2_CSS,
            },
        ]
    }

    pub(crate) fn draw_corners(&self, w: f32, h: f32) {
        let a = self.mood.glass();
        self.hud
            .set_stroke(&css_rgba(C_IRIS2.with_alpha(0.34 * a).into()));
        let len = 26.0;
        for (x, y, sx, sy) in [
            (11.0, 50.0, 1.0, 1.0),
            (w - 11.0, 50.0, -1.0, 1.0),
            (11.0, h - 11.0, 1.0, -1.0),
            (w - 11.0, h - 11.0, -1.0, -1.0),
        ] {
            self.line(x, y, x + sx * len, y);
            self.line(x, y, x, y + sy * len);
        }
    }

    pub(crate) fn draw_compass(&self, w: f32, h: f32) {
        let cx = w - 71.0;
        // On narrow canvases the bottom-left DOM status chip reaches the
        // compass's berth — lift the dial above the chip band (ST-02).
        // The glass disc spans cy ± 18, so the lifted bottom edge lands
        // just above h − STATUS_CHIP_CLEARANCE.
        let cy = if status_chip_reaches(w, cx - 18.0) {
            h - STATUS_CHIP_CLEARANCE - 19.0
        } else {
            h - 33.0
        };
        // Small glass disc so the dial reads over any scene behind it.
        self.hud.ctx.begin_path();
        let _ = self
            .hud
            .ctx
            .arc(cx as f64, cy as f64, 18.0, 0.0, std::f64::consts::TAU);
        self.hud.set_fill("rgba(13,14,24,0.55)");
        self.hud.ctx.fill();
        self.hud.set_stroke(&css_rgba(
            C_IRIS2.with_alpha(0.40 * self.mood.glass()).into(),
        ));
        self.hud.ctx.stroke();
        let angle = -self.yaw as f64;
        self.hud.set_stroke(C_IRIS_CSS);
        self.hud.ctx.begin_path();
        self.hud.ctx.move_to(cx as f64, cy as f64);
        self.hud.ctx.line_to(
            cx as f64 + angle.sin() * 14.0,
            cy as f64 - angle.cos() * 14.0,
        );
        self.hud.ctx.stroke();
        self.text("N", cx + 27.0, cy + 4.0, 10.0, C_TEXT3_CSS, "bold");
    }

    pub(crate) fn meter(&self, x: f32, y: f32, w: f32, pct: f32, color: &str) {
        let pct = pct.clamp(0.0, 1.0);
        self.hud.set_fill("rgba(26,30,40,0.92)");
        self.hud
            .ctx
            .fill_rect(x as f64, (y - 6.0) as f64, w as f64, 5.0);
        self.hud.set_fill(color);
        self.hud
            .ctx
            .fill_rect(x as f64, (y - 6.0) as f64, (w * pct) as f64, 5.0);
        self.hud.set_stroke("rgba(126,136,150,0.5)");
        self.hud
            .ctx
            .stroke_rect(x as f64, (y - 6.0) as f64, w as f64, 5.0);
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn pill_button(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        label: &str,
        active: bool,
        action: HitAction,
    ) {
        self.pill_at(
            x,
            y,
            w,
            h,
            label,
            if active { C_IRIS_CSS } else { C_TEXT3_CSS },
            active,
        );
        self.hit_zones.push(HitZone::new(x, y, w, h, action));
    }

    /// Capsule pill with the glass treatment. `active` (selected) and
    /// hovered pills are lit from within: an accent gradient swelling from
    /// the capsule's middle plus a brighter luminous border.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn pill_at(
        &self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        label: &str,
        color: &str,
        active: bool,
    ) {
        let ctx = &self.hud.ctx;
        let a = self.mood.glass();
        let accent = hex_color(color).unwrap_or(C_TEXT3);
        let hovered = self
            .hover_xy
            .is_some_and(|(hx, hy)| hx >= x && hx <= x + w && hy >= y && hy <= y + h);
        let r = (h * 0.5).min(11.0);
        // Dark translucent capsule base.
        self.rounded_path(x, y, w, h, r);
        let base = ctx.create_linear_gradient(x as f64, y as f64, x as f64, (y + h) as f64);
        let _ = base.add_color_stop(
            0.0,
            &css_rgba(Color::rgb(42, 44, 66).with_alpha(0.52).into()),
        );
        let _ = base.add_color_stop(
            1.0,
            &css_rgba(Color::rgb(13, 14, 24).with_alpha(0.68).into()),
        );
        ctx.set_fill_style_canvas_gradient(&base);
        self.hud.note_fill_unknown();
        ctx.fill();
        if active || hovered {
            let lit = (if active { 0.30 } else { 0.20 }) * a;
            let inner = ctx.create_linear_gradient(x as f64, y as f64, x as f64, (y + h) as f64);
            let _ = inner.add_color_stop(0.0, &css_rgba(accent.with_alpha(lit * 0.35).into()));
            let _ = inner.add_color_stop(0.5, &css_rgba(accent.with_alpha(lit).into()));
            let _ = inner.add_color_stop(1.0, &css_rgba(accent.with_alpha(lit * 0.45).into()));
            ctx.set_fill_style_canvas_gradient(&inner);
            ctx.fill();
        }
        // Gentle top highlight, then the luminous border.
        self.hud.set_stroke(&css_rgba([0.93, 0.95, 1.0, 0.07 * a]));
        self.line(x + r, y + 1.0, x + w - r, y + 1.0);
        let border = if active {
            0.85
        } else if hovered {
            0.62
        } else {
            0.38
        } * a;
        self.rounded_path(x, y, w, h, r);
        self.hud
            .set_stroke(&css_rgba(accent.with_alpha(border).into()));
        ctx.stroke();
        self.text(label, x + 8.0, y + h * 0.65, 10.0, color, "bold");
    }

    /// Trace a rounded-rect path on the HUD context (no fill/stroke).
    pub(crate) fn rounded_path(&self, x: f32, y: f32, w: f32, h: f32, r: f32) {
        let ctx = &self.hud.ctx;
        let r = r.min(w * 0.5).min(h * 0.5).max(0.0);
        ctx.begin_path();
        ctx.move_to((x + r) as f64, y as f64);
        ctx.line_to((x + w - r) as f64, y as f64);
        ctx.quadratic_curve_to((x + w) as f64, y as f64, (x + w) as f64, (y + r) as f64);
        ctx.line_to((x + w) as f64, (y + h - r) as f64);
        ctx.quadratic_curve_to(
            (x + w) as f64,
            (y + h) as f64,
            (x + w - r) as f64,
            (y + h) as f64,
        );
        ctx.line_to((x + r) as f64, (y + h) as f64);
        ctx.quadratic_curve_to(x as f64, (y + h) as f64, x as f64, (y + h - r) as f64);
        ctx.line_to(x as f64, (y + r) as f64);
        ctx.quadratic_curve_to(x as f64, y as f64, (x + r) as f64, y as f64);
        ctx.close_path();
    }

    /// Frosted-glass panel, canvas-native: a soft outer shadow, layered
    /// translucent body gradient, a top-edge specular sheen, a faint inner
    /// highlight, and a 1px luminous border with corner glow. Everything is
    /// plain gradient/alpha layering — no `ctx.filter` blur, which would be
    /// far too slow to repaint per frame.
    ///
    /// `emphasis` scales the accent (border/corner) luminosity — ~1.0 for
    /// resting panels, higher for selected/featured ones. `tint` scales the
    /// body opacity — 1.0 for solid panels, low values for see-through
    /// surfaces that must not hide the 3D scene behind them. The calm mood
    /// additionally dims all accents via [`Mood::glass`].
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn glass_panel(
        &self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        r: f32,
        accent: Color,
        emphasis: f32,
        tint: f32,
    ) {
        let ctx = &self.hud.ctx;
        let a = self.mood.glass();
        // Soft outer shadow: one slightly enlarged, downward-biased dark
        // fill fakes a blurred drop shadow.
        self.rounded_path(x - 2.0, y - 1.0, w + 4.0, h + 5.0, r + 3.0);
        self.hud.set_fill("rgba(2,3,9,0.30)");
        ctx.fill();
        // Body: deep dark vertical gradient (lighter up top, denser below).
        self.rounded_path(x, y, w, h, r);
        let body = ctx.create_linear_gradient(x as f64, y as f64, x as f64, (y + h) as f64);
        let _ = body.add_color_stop(
            0.0,
            &css_rgba(Color::rgb(38, 40, 60).with_alpha(0.62 * tint).into()),
        );
        let _ = body.add_color_stop(
            0.45,
            &css_rgba(Color::rgb(21, 22, 34).with_alpha(0.74 * tint).into()),
        );
        let _ = body.add_color_stop(
            1.0,
            &css_rgba(Color::rgb(12, 12, 20).with_alpha(0.85 * tint).into()),
        );
        ctx.set_fill_style_canvas_gradient(&body);
        self.hud.note_fill_unknown();
        ctx.fill();
        // Top-edge specular sheen; the body path is still current.
        let sheen_h = (h * 0.42).clamp(8.0, 30.0);
        let sheen = ctx.create_linear_gradient(x as f64, y as f64, x as f64, (y + sheen_h) as f64);
        let _ = sheen.add_color_stop(0.0, &css_rgba([0.92, 0.95, 1.0, 0.10 * a]));
        let _ = sheen.add_color_stop(1.0, "rgba(235,242,255,0)");
        ctx.set_fill_style_canvas_gradient(&sheen);
        ctx.fill();
        // Gentle inner highlight stroke, inset 1px.
        self.rounded_path(
            x + 1.0,
            y + 1.0,
            (w - 2.0).max(1.0),
            (h - 2.0).max(1.0),
            (r - 1.0).max(1.5),
        );
        self.hud.set_stroke(&css_rgba([0.93, 0.95, 1.0, 0.05 * a]));
        ctx.stroke();
        // 1px luminous border.
        let border = ((0.26 + 0.26 * emphasis) * a).min(0.92);
        self.rounded_path(x, y, w, h, r);
        self.hud
            .set_stroke(&css_rgba(accent.with_alpha(border).into()));
        ctx.stroke();
        // Corner glow: brighter quarter-arcs hugging each rounded corner.
        let glow = (0.55 * emphasis * a).min(0.95);
        self.hud
            .set_stroke(&css_rgba(accent.with_alpha(glow).into()));
        let cr = r.max(2.0).min(w * 0.5).min(h * 0.5) as f64;
        let half_pi = std::f64::consts::FRAC_PI_2;
        for (cx, cy, start) in [
            (x + cr as f32, y + cr as f32, std::f64::consts::PI),
            (x + w - cr as f32, y + cr as f32, 1.5 * std::f64::consts::PI),
            (x + w - cr as f32, y + h - cr as f32, 0.0),
            (x + cr as f32, y + h - cr as f32, half_pi),
        ] {
            ctx.begin_path();
            let _ = ctx.arc(cx as f64, cy as f64, cr, start, start + half_pi);
            ctx.stroke();
        }
    }

    pub(crate) fn text(&self, text: &str, x: f32, y: f32, px: f32, color: &str, weight: &str) {
        self.hud.set_fill(color);
        self.hud.set_font(px, weight == "bold");
        let _ = self.hud.ctx.fill_text(text, x as f64, y as f64);
    }

    pub(crate) fn line(&self, x1: f32, y1: f32, x2: f32, y2: f32) {
        self.hud.ctx.begin_path();
        self.hud.ctx.move_to(x1 as f64, y1 as f64);
        self.hud.ctx.line_to(x2 as f64, y2 as f64);
        self.hud.ctx.stroke();
    }

    pub(crate) fn css_width(&self) -> f32 {
        self.width as f32 / self.dpr as f32
    }

    pub(crate) fn css_height(&self) -> f32 {
        self.height as f32 / self.dpr as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_line_wraps_words_and_hard_breaks_long_tokens() {
        assert_eq!(wrap_line("short line", 20), vec!["short line"]);
        let wrapped = wrap_line("alpha beta gamma delta epsilon", 11);
        assert!(
            wrapped.iter().all(|l| l.chars().count() <= 11),
            "{wrapped:?}"
        );
        assert_eq!(wrapped.join(" "), "alpha beta gamma delta epsilon");
        // Overlong tokens hard-break instead of overflowing the panel.
        let token = "a".repeat(30);
        let broken = wrap_line(&token, 10);
        assert_eq!(broken.len(), 3);
        assert!(broken.iter().all(|l| l.chars().count() <= 10));
        // Empty input still yields one (empty) line.
        assert_eq!(wrap_line("", 10), vec![""]);
    }

    #[test]
    fn transcript_layout_assigns_monotonic_offsets_and_marks_first_lines() {
        let transcript = crate::model::StationTranscript {
            session_id: "s1".into(),
            rows: vec![
                crate::model::StationTranscriptRow {
                    kind: "user".into(),
                    ts: "12:00".into(),
                    text: "do the thing".into(),
                },
                crate::model::StationTranscriptRow {
                    kind: "model".into(),
                    ts: "12:01".into(),
                    text: "first paragraph that is long enough to wrap across lines\nsecond line"
                        .into(),
                },
            ],
            ..Default::default()
        };
        let layout = layout_transcript(&transcript, 24);
        assert!(layout.lines.len() >= 4);
        assert!(layout.lines[0].first);
        assert_eq!(layout.lines[0].kind, "user");
        // Exactly one `first` line per row.
        assert_eq!(layout.lines.iter().filter(|l| l.first).count(), 2);
        // Offsets strictly increase and content height covers them all.
        for pair in layout.lines.windows(2) {
            assert!(pair[1].y > pair[0].y);
        }
        assert!(layout.content_h >= layout.lines.last().unwrap().y);
        // Signature changes when content grows.
        let sig_a = transcript_signature(&transcript, 24);
        let mut grown = transcript.clone();
        grown.rows.push(crate::model::StationTranscriptRow {
            kind: "agent".into(),
            ts: String::new(),
            text: "tail".into(),
        });
        assert_ne!(sig_a, transcript_signature(&grown, 24));
        assert_ne!(sig_a, transcript_signature(&transcript, 30));
    }
}
