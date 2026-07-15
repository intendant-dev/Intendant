//! The station scene stage: the top-level `draw_hud` pass, vignette,
//! display thumbnails and video paint-through, the station header,
//! control center and command deck, the compact surface, the orbital
//! scene core, the activity lane, and focus-detail dispatch.

use super::*;

impl StationInner {
    pub(crate) fn draw_hud(&mut self, time_ms: f64) {
        self.hud
            .ctx
            .set_transform(self.dpr, 0.0, 0.0, self.dpr, 0.0, 0.0)
            .ok();
        let w = self.css_width();
        let h = self.css_height();
        self.hud.ctx.clear_rect(0.0, 0.0, w as f64, h as f64);
        self.hit_zones.clear();
        self.scroll_zones.clear();
        self.composer_input_rect = None;

        if self.gpu.is_none() && self.scene_ctx.is_none() {
            // Runtime WebGPU failure with a consumed scene canvas: paint the
            // wireframe under the HUD. The identity transform matches the
            // device-pixel coordinates draw_scene_lines expects.
            self.hud.ctx.save();
            self.hud
                .ctx
                .set_transform(1.0, 0.0, 0.0, 1.0, 0.0, 0.0)
                .ok();
            self.draw_scene_lines(&self.hud.ctx);
            self.hud.ctx.restore();
            self.hud.invalidate_styles();
        }

        self.draw_vignette(w, h);
        self.draw_display_thumbnails();
        self.draw_station_header(w);
        self.draw_station_control_center(w, h, time_ms);
        self.draw_corners(w, h);
        self.draw_compass(w, h);
        if let Some(id) = self.selected_id.clone() {
            self.draw_station_focus_detail(&id, w, h);
        }
        // The transcript viewer and composer float above everything else
        // (drawn last = clicked first).
        if self.transcript.is_some() {
            self.draw_transcript_panel(w, h);
        }
        if self.composer_open {
            self.draw_composer_strip(w, h);
        }
    }

    pub(crate) fn draw_vignette(&self, w: f32, h: f32) {
        if let Some(gradient) = self.hud.vignette(w, h, self.mood) {
            self.hud.ctx.set_fill_style_canvas_gradient(&gradient);
            self.hud.note_fill_unknown();
            self.hud.ctx.fill_rect(0.0, 0.0, w as f64, h as f64);
        }
    }

    /// Thumbnail frame rect (CSS px) for the `index`-th of `count` display
    /// sources anchored at the projected host position. Multi-display
    /// hosts fan out horizontally around the anchor instead of stacking
    /// every thumbnail on the same rect. Shared by the full HUD paint and
    /// the video-only partial repaint so the two can never drift apart.
    pub(crate) fn thumbnail_rect(
        css: Vec2,
        css_width: f32,
        index: usize,
        count: usize,
    ) -> ThumbRect {
        let tw = 164.0_f32.min(css_width * 0.28).max(98.0);
        let th = tw * 0.5625;
        let fan = (index as f32 - count.saturating_sub(1) as f32 * 0.5) * (tw + 10.0);
        let x = css.x - tw / 2.0 + fan;
        let y = css.y - 118.0 - th * 0.2;
        (x, y, tw, th)
    }

    /// Projected host nodes by bare host id, for anchoring display
    /// thumbnails to their hosts.
    pub(crate) fn host_nodes(&self) -> HashMap<&str, &ProjectedNode> {
        self.frame
            .projected_nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Host)
            .map(|n| (n.id.strip_prefix("host:").unwrap_or(n.id.as_str()), n))
            .collect()
    }

    /// CSS-px center of a projected node.
    pub(crate) fn node_css_center(&self, node: &ProjectedNode) -> Vec2 {
        let center = ndc_to_screen([node.ndc.x, node.ndc.y], self.width, self.height);
        Vec2::new(center.x / self.dpr as f32, center.y / self.dpr as f32)
    }

    /// Every display source with its placed thumbnail rect. Sources are
    /// sorted by id (HashMap order would make multi-display fans jitter
    /// between paints) and indexed per host for the fan-out.
    pub(crate) fn placed_display_thumbnails(&self) -> Vec<(&crate::DisplaySource, ThumbRect)> {
        if self.display_sources.is_empty() {
            return Vec::new();
        }
        let by_host = self.host_nodes();
        let mut sources: Vec<(&String, &crate::DisplaySource)> =
            self.display_sources.iter().collect();
        sources.sort_by(|a, b| a.0.cmp(b.0));
        let mut per_host_count: HashMap<&str, usize> = HashMap::new();
        for (_, source) in &sources {
            *per_host_count.entry(source.host_id.as_str()).or_default() += 1;
        }
        let css_w = self.css_width();
        let mut per_host_seen: HashMap<&str, usize> = HashMap::new();
        let mut placed = Vec::with_capacity(sources.len());
        for (_, source) in sources {
            let Some(node) = by_host.get(source.host_id.as_str()) else {
                continue;
            };
            let count = per_host_count
                .get(source.host_id.as_str())
                .copied()
                .unwrap_or(1);
            let seen = per_host_seen.entry(source.host_id.as_str()).or_default();
            let index = *seen;
            *seen += 1;
            let css = self.node_css_center(node);
            placed.push((source, Self::thumbnail_rect(css, css_w, index, count)));
        }
        placed
    }

    /// Partial HUD repaint: refresh only the live video pixels inside the
    /// already-painted thumbnail frames. Valid whenever nothing else on
    /// the HUD changed since the last full paint (`render` guarantees the
    /// camera is unchanged, so the cached frame geometry still matches):
    /// the glass frame, label, and every other panel stay as previously
    /// rasterized, and the opaque video pixels overwrite themselves in
    /// place — no clearing, no translucent-fill accumulation.
    pub(crate) fn paint_display_videos(&self) {
        if self.display_sources.is_empty() {
            return;
        }
        self.hud
            .ctx
            .set_transform(self.dpr, 0.0, 0.0, self.dpr, 0.0, 0.0)
            .ok();
        for (source, (x, y, tw, th)) in self.placed_display_thumbnails() {
            // Sources still waiting for pixels keep their painted
            // placeholder; the first ready frame simply draws over it.
            if source.video.video_width() == 0 || source.video.video_height() == 0 {
                continue;
            }
            let _ = self
                .hud
                .ctx
                .draw_image_with_html_video_element_and_dw_and_dh(
                    &source.video,
                    (x + 3.0) as f64,
                    (y + 3.0) as f64,
                    (tw - 6.0) as f64,
                    (th - 6.0) as f64,
                );
        }
    }

    pub(crate) fn draw_display_thumbnails(&self) {
        for (source, (x, y, tw, th)) in self.placed_display_thumbnails() {
            self.glass_panel(x, y, tw, th, 6.0, C_AMBER, 1.2, 1.15);
            let video_ready = source.video.video_width() > 0 && source.video.video_height() > 0;
            if video_ready {
                let _ = self
                    .hud
                    .ctx
                    .draw_image_with_html_video_element_and_dw_and_dh(
                        &source.video,
                        (x + 3.0) as f64,
                        (y + 3.0) as f64,
                        (tw - 6.0) as f64,
                        (th - 6.0) as f64,
                    );
            } else {
                self.hud.set_fill("rgba(26,30,40,0.55)");
                self.hud.ctx.fill_rect(
                    (x + 3.0) as f64,
                    (y + 3.0) as f64,
                    (tw - 6.0) as f64,
                    (th - 6.0) as f64,
                );
                self.text(
                    "linking display",
                    x + 12.0,
                    y + th / 2.0,
                    10.0,
                    C_TEXT3_CSS,
                    "normal",
                );
            }
            self.text(
                &source.label,
                x + 7.0,
                y + th + 12.0,
                10.0,
                C_AMBER_CSS,
                "normal",
            );
        }
    }

    pub(crate) fn draw_station_header(&mut self, w: f32) {
        let ctx = &self.hud.ctx;
        let a = self.mood.glass();
        // Full-bleed glass strip: translucent gradient body, top sheen,
        // luminous bottom edge.
        let body = ctx.create_linear_gradient(0.0, 0.0, 0.0, 42.0);
        let _ = body.add_color_stop(0.0, "rgba(16,17,28,0.92)");
        let _ = body.add_color_stop(1.0, "rgba(11,11,19,0.62)");
        ctx.set_fill_style_canvas_gradient(&body);
        self.hud.note_fill_unknown();
        ctx.fill_rect(0.0, 0.0, w as f64, 42.0);
        self.hud.set_stroke(&css_rgba([0.93, 0.95, 1.0, 0.06 * a]));
        self.line(0.0, 1.0, w, 1.0);
        self.hud
            .set_stroke(&css_rgba(C_IRIS.with_alpha(0.30 * a).into()));
        self.line(0.0, 42.0, w, 42.0);
        self.text("STATION", 24.0, 26.0, 11.0, C_TEXT_CSS, "bold");
        self.pill_button(
            96.0,
            10.0,
            78.0,
            23.0,
            "orbital",
            self.layout == LayoutName::Orbital,
            HitAction::Layout(LayoutName::Orbital),
        );
        self.pill_button(
            182.0,
            10.0,
            116.0,
            23.0,
            "constellation",
            self.layout == LayoutName::Constellation,
            HitAction::Layout(LayoutName::Constellation),
        );

        // Attention alert strip: the snapshot's attention queue surfaces in
        // the header so blocked work is visible from any layout. Click
        // selects system:controls, whose focus panel lists the items.
        let mut status_x = 318.0;
        let queue = &self.snapshot.attention_queue;
        if queue.count > 0 {
            let color = if queue.blocked > 0 {
                C_ROSE_CSS
            } else {
                C_AMBER_CSS
            };
            let top = queue
                .items
                .first()
                .map(|item| truncate(&item.title, 22))
                .unwrap_or_default();
            let label = if top.is_empty() {
                format!("{} attention", queue.count)
            } else {
                format!("{} attention / {top}", queue.count)
            };
            let pill_w = (label.chars().count() as f32 * 6.1 + 18.0).min(w * 0.34);
            self.pill_at(status_x, 10.0, pill_w, 23.0, &label, color, true);
            self.hit_zones.push(HitZone::new(
                status_x,
                10.0,
                pill_w,
                23.0,
                HitAction::Select("system:controls".to_string()),
            ));
            status_x += pill_w + 12.0;
        }

        let active_agents = self
            .snapshot
            .agents
            .iter()
            .filter(|agent| agent.status == "in_progress")
            .count();
        let pending = self
            .snapshot
            .agents
            .iter()
            .filter(|agent| agent.needs_approval)
            .count();
        let right = format!(
            "{} hosts / {} active / {} approvals / renderer {}",
            self.snapshot.hosts.len(),
            active_agents,
            pending,
            if self.gpu.is_some() {
                "WebGPU"
            } else {
                "Canvas"
            },
        );
        self.text(
            &truncate(&right, ((w - status_x - 12.0) / 7.0).max(22.0) as usize),
            status_x,
            26.0,
            10.0,
            if pending > 0 {
                C_AMBER_CSS
            } else {
                C_TEXT2_CSS
            },
            "normal",
        );
    }

    pub(crate) fn draw_station_control_center(&mut self, w: f32, h: f32, time_ms: f64) {
        if w < 360.0 || h < 320.0 {
            return;
        }
        if w < 820.0 {
            self.draw_station_compact_surface(w, h);
            return;
        }

        let margin = 24.0;
        let top_y = 58.0;
        let command_h = if h < 640.0 { 78.0 } else { 92.0 };
        let (center_x, main_y, center_w, main_h) = scene_core_rect(w, h, self.density);

        self.draw_station_command_deck(margin, top_y, center_w, command_h);
        self.draw_station_scene_core(center_x, main_y, center_w, main_h, time_ms);
        self.draw_station_activity_lane(margin, h, center_w);
    }

    pub(crate) fn draw_station_command_deck(&mut self, x: f32, y: f32, w: f32, h: f32) {
        self.glass_panel(x - 6.0, y - 8.0, w + 12.0, h + 14.0, 12.0, C_IRIS, 0.9, 0.9);
        self.hud.set_fill(C_IRIS_CSS);
        self.hud
            .ctx
            .fill_rect(x as f64, (y + 15.0) as f64, 3.0, 38.0);
        self.text(
            "CONTROL CENTER",
            x + 18.0,
            y + 24.0,
            12.0,
            C_IRIS_CSS,
            "bold",
        );
        self.text(
            &truncate(
                &self.station_target_label(),
                ((w * 0.44) / 7.0).max(38.0) as usize,
            ),
            x + 18.0,
            y + 48.0,
            14.0,
            C_TEXT_CSS,
            "bold",
        );

        let controls = &self.snapshot.controls;
        let session_state = if controls.session_detached {
            "detached"
        } else if controls.session_active {
            "active"
        } else if controls.session_id.is_empty() {
            "no target"
        } else {
            "idle"
        };
        let goal_status = controls.session_goal_status.trim();
        let mut session_line = format!(
            "{} / {} / {} / {}",
            nonempty(&controls.backend, "agent"),
            if controls.direct_mode {
                "direct"
            } else {
                "presence"
            },
            nonempty(&controls.approval_policy, "approval"),
            session_state
        );
        // The prompt target's standing order belongs on the command deck:
        // a full goal line when the tall deck has room, a short marker in
        // the session line otherwise.
        let tall_deck = h >= 90.0;
        if !goal_status.is_empty() && !tall_deck {
            session_line.push_str(&format!(" / goal {goal_status}"));
        }
        self.text(
            &truncate(&session_line, ((w * 0.46) / 6.2).max(42.0) as usize),
            x + 18.0,
            y + 68.0,
            10.0,
            C_TEXT2_CSS,
            "normal",
        );
        if !goal_status.is_empty() && tall_deck {
            let goal_color = goal_status_color_css(goal_status);
            let mut goal_line = format!(
                "goal {}: {}",
                goal_status,
                nonempty(&controls.session_goal_objective, "(no objective)")
            );
            if !controls.session_goal_tokens.trim().is_empty() {
                goal_line.push_str(&format!(" ({} tok)", controls.session_goal_tokens.trim()));
            }
            self.text(
                &truncate(&goal_line, ((w * 0.46) / 5.6).max(46.0) as usize),
                x + 18.0,
                y + 84.0,
                9.5,
                goal_color,
                "normal",
            );
        }

        let context_pct = percent(
            self.snapshot.context.tokens,
            self.snapshot.context.effective_window,
        );
        let managed_pct = percent(
            self.snapshot.managed.used_tokens,
            self.snapshot.managed.effective_window,
        );
        let metric_w = ((w * 0.42) - 24.0).max(300.0) / 3.0;
        let metric_x = x + w - metric_w * 3.0 - 18.0;
        let metric_y = y + 15.0;
        let metrics = [
            (
                "Context",
                pct_label(context_pct),
                pressure_color(context_pct),
            ),
            (
                "Managed",
                nonempty(&self.snapshot.managed.status, "unknown"),
                pressure_color(managed_pct),
            ),
            (
                "Changes",
                if self.snapshot.changes.count > 0 {
                    format!("{} files", self.snapshot.changes.count)
                } else {
                    nonempty(&self.snapshot.changes.status, "clean")
                },
                if self.snapshot.changes.count > 0 {
                    C_AMBER_CSS
                } else {
                    C_GREEN_CSS
                },
            ),
        ];
        for (idx, (label, value, color)) in metrics.into_iter().enumerate() {
            let mx = metric_x + idx as f32 * metric_w;
            self.text(label, mx + 10.0, metric_y + 15.0, 8.5, C_TEXT3_CSS, "bold");
            self.text(
                &truncate(&value, ((metric_w - 22.0) / 6.0).max(8.0) as usize),
                mx + 10.0,
                metric_y + 32.0,
                10.0,
                color,
                "bold",
            );
            let pct = if label == "Context" {
                context_pct
            } else if label == "Managed" {
                managed_pct
            } else if self.snapshot.changes.count > 0 {
                1.0
            } else {
                0.0
            };
            self.meter(mx + 10.0, metric_y + 39.0, metric_w - 28.0, pct, color);
        }

        let mut ax = x + w - 18.0;
        let ay = y + h - 34.0;
        // Keep the FIRST seven actions (send / new session lead the vec) and
        // lay them out right-to-left so the primaries sit nearest the corner;
        // capability-driven extras (select shortcuts) get dropped under
        // pressure — previously `.rev().take(7)` dropped the primaries.
        for action in self.station_primary_actions().into_iter().take(7).rev() {
            ax -= action.width;
            if ax < x + w * 0.48 {
                break;
            }
            self.pill_at(
                ax,
                ay,
                action.width,
                23.0,
                action.label,
                action.color,
                false,
            );
            self.hit_zones
                .push(HitZone::new(ax, ay, action.width, 23.0, action.hit));
            ax -= 8.0;
        }
    }

    pub(crate) fn draw_station_compact_surface(&mut self, w: f32, h: f32) {
        let x = 18.0;
        let y = 64.0;
        let panel_w = w - 36.0;
        let panel_h = (h - 92.0).max(180.0);
        self.glass_panel(x, y, panel_w, panel_h, 10.0, C_IRIS, 1.0, 1.0);
        self.text(
            "CONTROL CENTER",
            x + 16.0,
            y + 24.0,
            12.0,
            C_IRIS_CSS,
            "bold",
        );
        self.text(
            &truncate(&self.station_target_label(), 48),
            x + 16.0,
            y + 46.0,
            11.0,
            C_TEXT_CSS,
            "normal",
        );

        let targets = std::mem::take(&mut self.system_targets);
        let (count, pitch, tile_h) = compact_grid(self.density, panel_h);
        let tile_w = (panel_w - 44.0) * 0.5;
        let mut tx = x + 14.0;
        let mut ty = y + 66.0;
        for (idx, target) in targets.iter().take(count).enumerate() {
            if idx > 0 && idx % 2 == 0 {
                tx = x + 14.0;
                ty += pitch;
            }
            self.station_focus_button(tx, ty, tile_w, tile_h, target);
            tx += tile_w + 16.0;
        }
        self.system_targets = targets;
    }

    pub(crate) fn draw_station_scene_core(&mut self, x: f32, y: f32, w: f32, h: f32, time_ms: f64) {
        let (cx, cy, ring_scale, core_h) = scene_core_metrics(x, y, w, h);
        if core_h < 150.0 {
            return;
        }
        // Clear glass: low tint so the 3D scene stays visible through it.
        self.glass_panel(x, y, w, core_h, 12.0, C_IRIS2, 0.5, 0.28);
        self.hud.set_stroke(match self.mood {
            Mood::Cockpit => "rgba(126,140,250,0.28)",
            Mood::Calm => "rgba(126,140,250,0.18)",
        });
        let breathe = (time_ms as f32 * 0.001).sin() * 2.0 * self.mood.pulse();
        for radius in [ring_scale * 0.36, ring_scale * 0.62, ring_scale] {
            self.hud.ctx.begin_path();
            let _ = self.hud.ctx.arc(
                cx as f64,
                cy as f64,
                (radius + breathe) as f64,
                0.0,
                std::f64::consts::TAU,
            );
            self.hud.ctx.stroke();
        }
        self.text("LIVE STATE", x + 18.0, y + 24.0, 10.0, C_TEXT3_CSS, "bold");
        let targets = std::mem::take(&mut self.system_targets);
        let selected = self
            .selected_id
            .as_deref()
            .and_then(|id| targets.iter().find(|target| target.id == id));
        if let Some(target) = selected {
            self.text(
                target.title,
                x + 118.0,
                y + 24.0,
                10.0,
                target.color,
                "bold",
            );
            self.text(
                &truncate(&target.detail, ((w - 260.0) / 6.0).max(24.0) as usize),
                x + 210.0,
                y + 24.0,
                9.0,
                C_TEXT2_CSS,
                "normal",
            );
        }
        self.text(
            &format!(
                "{} events / {} sessions / {} peers",
                self.snapshot.events.len(),
                self.snapshot.sessions.total,
                self.snapshot.hosts.len().saturating_sub(1),
            ),
            x + 18.0,
            y + 43.0,
            11.0,
            C_TEXT_CSS,
            "normal",
        );

        // World panes own their screen area: a summary card that would
        // paint over an in-scene panel is skipped for the frame (the HUD
        // canvas sits above the scene canvas, so it would otherwise cover
        // the pane). Canvas-fallback frames draw no solid panes, so
        // nothing is skipped there.
        let pane_rects = if self.gpu.is_some() {
            self.pane_css_rects()
        } else {
            Vec::new()
        };
        for slot in orbital_card_slots(x, y, w, h) {
            if let Some(target) = targets.iter().find(|target| target.id == slot.id) {
                if pane_rects.iter().any(|(_, px, py, pw, ph)| {
                    slot.x < px + pw
                        && slot.x + slot.w > *px
                        && slot.y < py + ph
                        && slot.y + slot.h > *py
                }) {
                    continue;
                }
                self.station_orbital_node(cx, cy, slot.x, slot.y, slot.w, slot.h, target);
            }
        }

        self.system_targets = targets;
        // The legacy invisible 3x3 "matrix" of system-target hit zones is
        // gone: it was never drawn, yet (being pushed last) it outranked the
        // visible orbital nodes in reverse hit-testing — clicks on the lower
        // half of visible nodes selected a different, invisible target. The
        // orbital nodes carry the same Select actions, and the DOM hotspot
        // overlay (positioned from hotspot_rects) covers keyboard access.
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn station_orbital_node(
        &mut self,
        cx: f32,
        cy: f32,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        target: &SystemTarget,
    ) {
        let selected = self.selected_id.as_deref() == Some(target.id);
        let hovered = self.hover_xy.is_some_and(|(hx, hy)| {
            hx >= x - 8.0 && hx <= x + w + 8.0 && hy >= y - 8.0 && hy <= y + h + 8.0
        });
        let is_display = target.id == "system:peers";
        let anchor_x = if x + w * 0.5 < cx { x + w } else { x };
        let anchor_y = y + h * 0.5;
        self.hud.set_stroke(if selected {
            target.color
        } else {
            "rgba(126,140,250,0.22)"
        });
        self.line(cx, cy, anchor_x, anchor_y);
        self.hud.set_fill(target.color);
        self.hud.ctx.begin_path();
        let _ = self.hud.ctx.arc(
            anchor_x as f64,
            anchor_y as f64,
            4.0,
            0.0,
            std::f64::consts::TAU,
        );
        self.hud.ctx.fill();
        self.hud.set_stroke(target.color);
        self.hud.ctx.begin_path();
        let _ = self.hud.ctx.arc(
            anchor_x as f64,
            anchor_y as f64,
            13.0,
            0.0,
            std::f64::consts::TAU,
        );
        self.hud.ctx.stroke();
        // Light glass chip behind the node text so it reads over the scene.
        self.glass_panel(
            x - 12.0,
            y - 4.0,
            w + 18.0,
            h + 8.0,
            9.0,
            hex_color(target.color).unwrap_or(C_IRIS),
            if selected {
                1.6
            } else if hovered {
                1.1
            } else {
                0.55
            },
            if selected { 0.95 } else { 0.62 },
        );
        if is_display {
            self.hud.set_stroke("rgba(228,168,91,0.58)");
            let aperture_w = (w * 0.34).max(92.0);
            let aperture_cx = x + aperture_w * 0.5;
            let aperture_cy = y + 29.0;
            for radius in [aperture_w * 0.22, aperture_w * 0.34] {
                self.hud.ctx.begin_path();
                let _ = self.hud.ctx.arc(
                    aperture_cx as f64,
                    aperture_cy as f64,
                    radius as f64,
                    0.0,
                    std::f64::consts::TAU,
                );
                self.hud.ctx.stroke();
            }
            self.text(
                target.kicker,
                x + aperture_w + 10.0,
                y + 15.0,
                8.0,
                C_TEXT3_CSS,
                "bold",
            );
            self.text(
                target.title,
                x + aperture_w + 10.0,
                y + 36.0,
                14.0,
                target.color,
                "bold",
            );
            self.text(
                &truncate(
                    &target.value,
                    ((w - aperture_w - 18.0) / 6.2).max(18.0) as usize,
                ),
                x + aperture_w + 10.0,
                y + 55.0,
                10.0,
                C_TEXT_CSS,
                "normal",
            );
            self.hit_zones.push(HitZone::new(
                x - 8.0,
                y - 8.0,
                w + 16.0,
                h + 16.0,
                HitAction::Select(target.id.to_string()),
            ));
            return;
        }
        self.text(target.kicker, x, y + 12.0, 8.0, C_TEXT3_CSS, "bold");
        self.text(target.title, x, y + 30.0, 12.0, target.color, "bold");
        self.text(
            &truncate(&target.value, ((w - 10.0) / 6.2).max(18.0) as usize),
            x,
            y + 47.0,
            10.0,
            C_TEXT_CSS,
            "normal",
        );
        if selected {
            self.text(
                &truncate(&target.detail, ((w - 10.0) / 6.4).max(18.0) as usize),
                x,
                y + h + 12.0,
                9.0,
                C_TEXT2_CSS,
                "normal",
            );
        }
        self.hit_zones.push(HitZone::new(
            x - 8.0,
            y - 8.0,
            w + 16.0,
            h + 16.0,
            HitAction::Select(target.id.to_string()),
        ));
    }

    pub(crate) fn draw_station_activity_lane(&mut self, x: f32, h: f32, w: f32) {
        let (rows, pitch, lane_h) = lane_metrics(self.density, h);
        // Glass panel extends lane_h + 10 below y, so this places the
        // panel bottom at h − STATUS_CHIP_CLEARANCE: the strip's last row
        // and border stay out of the DOM status chip's band (ST-02).
        let y = (h - lane_h - 10.0 - STATUS_CHIP_CLEARANCE).max(282.0);
        self.glass_panel(x - 6.0, y, w + 12.0, lane_h + 10.0, 12.0, C_SKY, 0.9, 0.9);
        self.hud.set_fill(C_SKY_CSS);
        self.hud
            .ctx
            .fill_rect((x + 1.0) as f64, (y + 18.0) as f64, 3.0, 34.0);
        self.text(
            "ACTIVITY RUNWAY",
            x + 18.0,
            y + 24.0,
            10.0,
            C_SKY_CSS,
            "bold",
        );
        let row_px = if rows > 3 { 8.5 } else { 9.0 };
        let latest = self
            .snapshot
            .events
            .iter()
            .rev()
            .take(rows)
            .collect::<Vec<_>>();
        if latest.is_empty() {
            self.text(
                "Waiting for retained activity",
                x + 18.0,
                y + 56.0,
                11.0,
                C_TEXT2_CSS,
                "normal",
            );
        } else {
            for (idx, event) in latest.into_iter().enumerate() {
                let row_y = y + 43.0 + idx as f32 * pitch;
                let color = level_color_css(&event.level);
                let row_rect = (x + 16.0, row_y - 11.0, w - 36.0, pitch - 1.0);
                let hovered = !event.session_id.is_empty()
                    && self.hover_xy.is_some_and(|(hx, hy)| {
                        hx >= row_rect.0
                            && hx <= row_rect.0 + row_rect.2
                            && hy >= row_rect.1
                            && hy <= row_rect.1 + row_rect.3
                    });
                if hovered {
                    self.rounded_path(row_rect.0, row_rect.1, row_rect.2, row_rect.3, 5.0);
                    self.hud.set_fill("rgba(93,169,230,0.10)");
                    self.hud.ctx.fill();
                }
                self.hud.set_fill(color);
                self.hud
                    .ctx
                    .fill_rect((x + 19.0) as f64, (row_y - 9.0) as f64, 4.0, 14.0);
                self.text(
                    &truncate(&nonempty(&event.ts, "--"), 10),
                    x + 33.0,
                    row_y,
                    row_px,
                    C_TEXT3_CSS,
                    "normal",
                );
                self.text(
                    &truncate(&event.level, 8),
                    x + 96.0,
                    row_y,
                    row_px,
                    color,
                    "bold",
                );
                self.text(
                    &truncate(&event.msg, ((w - 190.0) / 6.4).max(28.0) as usize),
                    x + 154.0,
                    row_y,
                    row_px,
                    C_TEXT2_CSS,
                    "normal",
                );
                // Runway rows with a session open that session's transcript.
                if !event.session_id.is_empty() {
                    self.hit_zones.push(HitZone::new(
                        row_rect.0,
                        row_rect.1,
                        row_rect.2,
                        row_rect.3,
                        HitAction::SessionAction {
                            action: "station-log".into(),
                            id: event.session_id.clone(),
                        },
                    ));
                }
            }
        }
        let actions = [
            LaneAction::activity("latest", "bottom", 68.0, C_SKY_CSS),
            LaneAction::activity("copy", "copy-visible", 56.0, C_IRIS_CSS),
            LaneAction::select("activity", "system:activity", 76.0, C_TEXT3_CSS),
        ];
        let mut ax = x + w - 18.0;
        for action in actions.into_iter().rev() {
            ax -= action.width;
            self.pill_at(
                ax,
                y + 13.0,
                action.width,
                22.0,
                action.label,
                action.color,
                false,
            );
            self.hit_zones
                .push(HitZone::new(ax, y + 13.0, action.width, 22.0, action.hit));
            ax -= 8.0;
        }
    }

    pub(crate) fn draw_station_focus_detail(&mut self, id: &str, w: f32, h: f32) {
        let panel_w = 460.0_f32.min(w - 48.0).max(280.0);
        let x = (w - panel_w - 24.0).max(24.0);
        // Sit just above the activity lane, wherever density placed it.
        let activity_lane_y = (h - lane_metrics(self.density, h).2 - 24.0).max(282.0);
        if let Some(agent) = self.snapshot.agents.iter().find(|a| a.id == id).cloned() {
            // Phase C slice 5: when the scene carried this agent's focus
            // as a world pane this frame (flag on, wide viewport — the
            // pane registered a pick target), and WebGPU actually renders
            // it (the canvas fallback draws lines only, so the pane would
            // be invisible there), the screen panel yields. The pane's
            // projected pill rects become this frame's hit zones, so
            // activate()-by-name, a11y hotspots, and rect picking keep
            // working over the in-scene pills.
            if self.gpu.is_some() && self.frame.pane_targets.iter().any(|t| t.id == id) {
                let zones: Vec<HitZone> = self
                    .frame
                    .pane_zones
                    .iter()
                    .map(|z| HitZone::new(z.x, z.y, z.w, z.h, z.action.clone()))
                    .collect();
                self.hit_zones.extend(zones);
                return;
            }
            self.draw_agent_focus(&agent, x, panel_w, activity_lane_y);
            return;
        }
        if let Some(host) = id
            .strip_prefix("host:")
            .and_then(|hid| self.snapshot.hosts.iter().find(|h| h.id == hid))
            .cloned()
        {
            self.draw_host_focus(&host, x, panel_w, activity_lane_y);
            return;
        }
        if id == "system:view" {
            self.draw_view_focus(x, panel_w, activity_lane_y);
            return;
        }
        // system:activity gets the full scrollable event panel like every
        // other system target (the runway below stays as the live ticker;
        // the panel adds history, filters, and per-event actions).
        if id.starts_with("system:") {
            let Some((title, value, detail, color)) = self
                .system_targets
                .iter()
                .find(|target| target.id == id)
                .map(|target| {
                    (
                        target.title.to_string(),
                        truncate(&target.value, 52),
                        truncate(&target.detail, 58),
                        target.color,
                    )
                })
            else {
                return;
            };
            let surface = self.system_panel_surface(id);
            // Tall actionable surface: anchored under the command deck,
            // down to the activity lane — rows scroll inside it.
            let command_h = if h < 640.0 { 78.0 } else { 92.0 };
            let top = if w < 820.0 {
                120.0
            } else {
                58.0 + command_h + 16.0
            };
            let panel_h = (activity_lane_y - 12.0 - top).max(220.0);
            let y = top;
            self.rows_panel(
                id, &title, color, &value, &detail, surface, x, y, panel_w, panel_h,
            );
            return;
        }
        let panel_h = 112.0;
        let y = (activity_lane_y - panel_h - 12.0).max(58.0);
        self.focus_panel_frame(x, y, panel_w, panel_h, "Selection", C_IRIS_CSS);
        self.text(
            &truncate(id, 52),
            x + 16.0,
            y + 68.0,
            11.0,
            C_TEXT_CSS,
            "normal",
        );
        self.text(
            "scene node selected",
            x + 16.0,
            y + 88.0,
            10.0,
            C_TEXT2_CSS,
            "normal",
        );
    }
}

/// Scene-core rect `(x, y, w, h)` — the space `draw_station_control_center`
/// hands `draw_station_scene_core` on a `css_w`×`css_h` canvas: full width
/// inside the 24px margins, between the command deck and the activity
/// lane. Pure so the orbital card placement tests exercise the exact
/// draw-path geometry; the deck-strip constants (24px margin, 58px top,
/// 78/92px deck, 14px gaps) mirror `draw_station_control_center`.
pub(crate) fn scene_core_rect(css_w: f32, css_h: f32, density: f32) -> (f32, f32, f32, f32) {
    let margin = 24.0;
    let top_y = 58.0;
    let gap = 14.0;
    let available_w = (css_w - margin * 2.0).max(760.0);
    let available_h = (css_h - top_y - 24.0).max(420.0);
    let command_h = if css_h < 640.0 { 78.0 } else { 92.0 };
    let lane_h = lane_metrics(density, css_h).2;
    let main_h = (available_h - command_h - lane_h - gap * 2.0).max(250.0);
    (margin, top_y + command_h + gap, available_w, main_h)
}

/// Shared scene-core metrics for a `(x, y, w, main_h)` rect: ring center
/// `(cx, cy)`, ring radius, and the clamped core height. One derivation
/// for the draw pass and the card placement, so they can never disagree.
pub(crate) fn scene_core_metrics(x: f32, y: f32, w: f32, main_h: f32) -> (f32, f32, f32, f32) {
    let core_h = main_h.clamp(330.0, 560.0);
    let cx = x + w * 0.5;
    let cy = y + core_h * 0.52;
    let ring = (core_h * 0.42).clamp(132.0, 230.0);
    (cx, cy, ring, core_h)
}

/// One placed system card in the orbital scene core (CSS px, logical
/// rect — the glass chrome and hit zone extend a little beyond it).
pub(crate) struct OrbitalSlot {
    pub(crate) id: &'static str,
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) w: f32,
    pub(crate) h: f32,
}

/// Minimum separations between orbital cards. The painted chrome
/// overshoots the logical rect (`station_orbital_node` draws its glass
/// at x−12..x+w+6, y−4..y+h+4) and the hit zone extends ±8px, so these
/// logical gaps keep both the pixels and the click targets apart.
const CARD_GAP_X: f32 = 28.0;
const CARD_GAP_Y: f32 = 20.0;
/// Inset from the scene-core panel edges (the pre-slot code clamped to
/// the same 20px).
const CARD_EDGE: f32 = 20.0;

/// Place the nine system cards inside the scene core `(x, y, w, main_h)`.
///
/// The previous hand-tuned polar offsets collided at common canvas sizes:
/// once `node_w` hit its 230px cap (any canvas ≥ ~1200px wide — observed
/// live at a 1440×900 window: Peers over Sessions, Context over Changes,
/// Controls over View/Worktrees). Positions are therefore derived from
/// the available space as flow rows with minimum gaps:
///
/// ```text
/// top band (y+58):        [ peers | sessions ]      one row centered on cx
/// middle band (~cy):  [activity]        [managed]   pinned outside the ring
/// tall panels, two bottom tiers stacked up from the panel bottom:
///   tier 1:               [ context ]  [ controls ]     flanking cx
///   tier 2:             [changes]  [ view ]  [worktrees]  rails + center
/// short panels (the tiers would cross the middle band — a 1440×900
/// window leaves the canvas ~580px tall and the core at its 330px floor):
///   one row:   [changes] [context] [ view ] [controls] [worktrees]
///              evenly spread across the panel, card width shrunk so the
///              gaps never drop below `CARD_GAP_X`
/// ```
///
/// Bands never share a vertical range (the middle band is floored below
/// the top band; the bottom tiers/row are floored below the middle band
/// and pinned inside the panel bottom — the short-mode row provably fits
/// at the 330px core floor), and cards within a band flow with at least
/// `CARD_GAP_X`, so no two cards can overlap at any full-surface size.
/// Unit-tested across 1280×520…2560×1440.
pub(crate) fn orbital_card_slots(x: f32, y: f32, w: f32, main_h: f32) -> [OrbitalSlot; 9] {
    let (cx, cy, ring, core_h) = scene_core_metrics(x, y, w, main_h);
    let node_w = (w * 0.20).clamp(158.0, 230.0);
    let node_h = 58.0;
    let wide_w = (node_w * 1.45).min(330.0);
    let wide_h = node_h + 16.0;

    // Top band: peers (wide) + sessions as one flow row centered on the
    // ring axis. The row always fits (max 588px vs the ≥760px panel), so
    // the edge clamp is defensive only.
    let top_y = y + 58.0;
    let top_x = (cx - (wide_w + CARD_GAP_X + node_w) * 0.5).max(x + CARD_EDGE);

    // Middle band: activity / managed flank the ring at its equator,
    // clamped inside the panel and floored below the top band (the ring
    // equator rides up into it once the core hits its height floor).
    // They can never meet each other: the ring keeps them a diameter
    // apart before clamping, and the clamps engage only on panels far
    // wider than two cards.
    let mid_y = (cy - node_h * 0.5).max(top_y + wide_h + CARD_GAP_Y);
    let activity_x = (cx - ring - 26.0 - node_w).max(x + CARD_EDGE);
    let managed_x = (cx + ring + 26.0).min(x + w - CARD_EDGE - node_w);

    let peers = OrbitalSlot {
        id: "system:peers",
        x: top_x,
        y: top_y,
        w: wide_w,
        h: wide_h,
    };
    let sessions = OrbitalSlot {
        id: "system:sessions",
        x: top_x + wide_w + CARD_GAP_X,
        y: top_y,
        w: node_w,
        h: node_h,
    };
    let activity = OrbitalSlot {
        id: "system:activity",
        x: activity_x,
        y: mid_y,
        w: node_w,
        h: node_h,
    };
    let managed = OrbitalSlot {
        id: "system:managed",
        x: managed_x,
        y: mid_y,
        w: node_w,
        h: node_h,
    };

    // Bottom band. Tall panels stack two tiers up from the panel bottom;
    // when the pinned upper tier would cross the middle band, collapse to
    // one evenly-spread row instead — a single row always fits above the
    // panel bottom at the 330px core floor (58 top offset + 74 peers +
    // 20 + 58 mid + 20 + 58 row + 20 edge = 308 < 330).
    let tier1_pinned = y + core_h - CARD_EDGE - 2.0 * node_h - CARD_GAP_Y;
    if tier1_pinned >= mid_y + node_h + CARD_GAP_Y {
        let tier2_y = tier1_pinned + node_h + CARD_GAP_Y;
        // Tier 1 flanks the ring axis symmetrically; tier 2 reuses the
        // middle band's rails (changes under activity, worktrees under
        // managed) with view centered — the rails and the center are
        // provably a card apart.
        let flank = ring * 0.45;
        let context_x = (cx - flank - node_w).max(x + CARD_EDGE);
        let controls_x = (cx + flank).min(x + w - CARD_EDGE - node_w);
        let bottom = |id, bx, by| OrbitalSlot {
            id,
            x: bx,
            y: by,
            w: node_w,
            h: node_h,
        };
        [
            peers,
            sessions,
            activity,
            managed,
            bottom("system:context", context_x, tier1_pinned),
            bottom("system:controls", controls_x, tier1_pinned),
            bottom("system:changes", activity_x, tier2_y),
            bottom("system:view", cx - node_w * 0.5, tier2_y),
            bottom("system:worktrees", managed_x, tier2_y),
        ]
    } else {
        let row_y = y + core_h - CARD_EDGE - node_h;
        let inner = w - 2.0 * CARD_EDGE;
        // Shrink the row's cards until five of them fit with full gaps,
        // then spread the leftover evenly (view lands centered on cx).
        let row_w = node_w.min((inner - 4.0 * CARD_GAP_X) / 5.0);
        let step = row_w + (inner - 5.0 * row_w) / 4.0;
        let row = |id, index: usize| OrbitalSlot {
            id,
            x: x + CARD_EDGE + index as f32 * step,
            y: row_y,
            w: row_w,
            h: node_h,
        };
        [
            peers,
            sessions,
            activity,
            managed,
            row("system:changes", 0),
            row("system:context", 1),
            row("system:view", 2),
            row("system:controls", 3),
            row("system:worktrees", 4),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Painted + interactive footprint of a card: the glass extends 12px
    /// left / 6px right / 4px vertically beyond the logical rect and the
    /// hit zone ±8px; the union is what must stay disjoint between cards.
    fn footprint(slot: &OrbitalSlot) -> (f32, f32, f32, f32) {
        (slot.x - 12.0, slot.y - 8.0, slot.w + 20.0, slot.h + 16.0)
    }

    fn overlaps(a: &(f32, f32, f32, f32), b: &(f32, f32, f32, f32)) -> bool {
        a.0 < b.0 + b.2 && a.0 + a.2 > b.0 && a.1 < b.1 + b.3 && a.1 + a.3 > b.1
    }

    /// The audited failure (a 1440×900 window: Peers over Sessions,
    /// Context over Changes — plus Controls over View/Worktrees by the
    /// same math) and the surrounding common sizes: no two cards may
    /// collide even counting their painted overhang and hit-zone outset,
    /// and every card stays inside the panel. Sizes are CANVAS CSS px —
    /// the dashboard chrome eats 150-320px of a window's height, so a
    /// 1440×900 window really hands the canvas ~580px (measured live);
    /// the short heights below cover exactly that regime.
    #[test]
    fn orbital_cards_do_not_collide_at_common_sizes() {
        for (w, h) in [
            (1280.0, 800.0),
            (1366.0, 768.0),
            (1440.0, 900.0),
            (1512.0, 982.0),
            (1536.0, 960.0),
            (1680.0, 1050.0),
            (1920.0, 1080.0),
            (2560.0, 1440.0),
            (1024.0, 768.0),
            (820.0, 700.0),
            // Chrome-shortened canvases (window height minus dashboard
            // chrome): the live 1440×900-window case and neighbors.
            (1440.0, 581.0),
            (1440.0, 620.0),
            (1280.0, 560.0),
            (1920.0, 640.0),
            (2560.0, 700.0),
        ] {
            for density in [0.5f32, 1.0, 1.8] {
                let (x, y, pw, main_h) = scene_core_rect(w, h, density);
                let slots = orbital_card_slots(x, y, pw, main_h);
                let (_, _, _, core_h) = scene_core_metrics(x, y, pw, main_h);
                for (i, a) in slots.iter().enumerate() {
                    for b in slots.iter().skip(i + 1) {
                        assert!(
                            !overlaps(&footprint(a), &footprint(b)),
                            "{w}x{h} d{density}: {} {:?} collides with {} {:?}",
                            a.id,
                            footprint(a),
                            b.id,
                            footprint(b),
                        );
                    }
                }
                for slot in &slots {
                    assert!(
                        slot.x >= x + CARD_EDGE - 0.5
                            && slot.x + slot.w <= x + pw - CARD_EDGE + 0.5,
                        "{w}x{h} d{density}: {} exits the panel horizontally",
                        slot.id,
                    );
                    assert!(
                        slot.y >= y && slot.y + slot.h <= y + core_h,
                        "{w}x{h} d{density}: {} exits the panel vertically ({}..{} vs {}..{})",
                        slot.id,
                        slot.y,
                        slot.y + slot.h,
                        y,
                        y + core_h,
                    );
                }
            }
        }
    }

    /// Regression pin for the audited pairs, at both the raw audited
    /// window size and the chrome-shortened canvas it actually yields.
    #[test]
    fn audited_pairs_are_separated_at_1440x900() {
        for h in [900.0, 581.0] {
            let (x, y, w, main_h) = scene_core_rect(1440.0, h, 1.0);
            let slots = orbital_card_slots(x, y, w, main_h);
            let rect = |id: &str| {
                slots
                    .iter()
                    .find(|slot| slot.id == id)
                    .map(footprint)
                    .unwrap_or_else(|| panic!("missing card {id}"))
            };
            for (a, b) in [
                ("system:peers", "system:sessions"),
                ("system:context", "system:changes"),
                ("system:controls", "system:worktrees"),
                ("system:controls", "system:view"),
                ("system:context", "system:view"),
            ] {
                assert!(
                    !overlaps(&rect(a), &rect(b)),
                    "{a} still collides with {b} at 1440x{h}"
                );
            }
        }
    }

    /// The placement covers all nine system targets exactly once, and the
    /// tall-mode band structure holds: peers/sessions share the top row,
    /// the bottom tiers sit strictly below the middle band and strictly
    /// apart.
    #[test]
    fn orbital_slots_cover_the_system_targets_in_bands() {
        let (x, y, w, main_h) = scene_core_rect(1440.0, 900.0, 1.0);
        let slots = orbital_card_slots(x, y, w, main_h);
        let mut ids: Vec<&str> = slots.iter().map(|slot| slot.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 9, "duplicate or missing system card ids");
        let slot = |id: &str| slots.iter().find(|slot| slot.id == id).unwrap();
        assert_eq!(slot("system:peers").y, slot("system:sessions").y);
        assert_eq!(slot("system:context").y, slot("system:controls").y);
        assert_eq!(slot("system:changes").y, slot("system:view").y);
        assert_eq!(slot("system:view").y, slot("system:worktrees").y);
        let mid_bottom = slot("system:activity").y + slot("system:activity").h;
        assert!(slot("system:context").y >= mid_bottom + CARD_GAP_Y);
        let tier1_bottom = slot("system:context").y + slot("system:context").h;
        assert!(slot("system:changes").y >= tier1_bottom + CARD_GAP_Y);
    }

    /// Short-mode structure (chrome-shortened canvas, core at its 330px
    /// floor): the five bottom cards collapse onto one row, evenly
    /// spread with at least the standard gap, view centered on the ring
    /// axis, and the row still clears the middle band.
    #[test]
    fn short_panels_collapse_the_bottom_band_to_one_row() {
        let (x, y, w, main_h) = scene_core_rect(1440.0, 581.0, 1.0);
        let slots = orbital_card_slots(x, y, w, main_h);
        let slot = |id: &str| {
            slots
                .iter()
                .find(|slot| slot.id == id)
                .unwrap_or_else(|| panic!("missing card {id}"))
        };
        let row = [
            slot("system:changes"),
            slot("system:context"),
            slot("system:view"),
            slot("system:controls"),
            slot("system:worktrees"),
        ];
        for card in &row[1..] {
            assert_eq!(card.y, row[0].y, "{} left the bottom row", card.id);
        }
        for pair in row.windows(2) {
            let gap = pair[1].x - (pair[0].x + pair[0].w);
            assert!(
                gap >= CARD_GAP_X - 0.5,
                "{}→{} gap {gap} under the minimum",
                pair[0].id,
                pair[1].id,
            );
        }
        // View stays centered on the ring axis.
        let (cx, ..) = scene_core_metrics(x, y, w, main_h);
        let view = slot("system:view");
        assert!((view.x + view.w * 0.5 - cx).abs() < 0.5);
        // The row clears the (floored) middle band by the standard gap.
        let mid = slot("system:activity");
        assert!(row[0].y >= mid.y + mid.h + CARD_GAP_Y);
    }
}
