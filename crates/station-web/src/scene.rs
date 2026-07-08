//! 3D scene: math primitives, camera, node layout, and frame building.

use std::collections::HashMap;
use std::f32::consts::PI;

use web_sys::CanvasRenderingContext2d;

use crate::gpu::GpuFrame;
use crate::input::HitAction;
use crate::model::{StationAgent, StationHost, StationSnapshot};
use crate::util::phase_color;
use crate::util::{
    css_color, css_rgba, epoch_seconds_now, goal_status_color, pressure_color, relationship_color,
    role_color, stable_angle, stable_unit, Color, C_BLUE, C_GREEN, C_PEACH, C_RED, C_SAPPHIRE,
    C_SUBTEXT0, C_SURFACE0, C_TEAL, C_TEXT, C_YELLOW,
};
use crate::StationInner;

impl StationInner {
    /// Refill `self.frame` for this frame, reusing its buffers. `anim_ms`
    /// drives ambient animation phases (frozen at motion 0); `time_ms` is
    /// real time, used for self-expiring event particles.
    pub(crate) fn build_frame(&mut self, anim_ms: f64, time_ms: f64) {
        let mut frame = std::mem::take(&mut self.frame);
        frame.clear();
        let camera = self.camera();
        // The camera moves into the projector closure below; panes need
        // its basis to billboard, so copy it out first.
        let (cam_right, cam_up) = (camera.right, camera.up);
        let aspect = self.width as f32 / self.height.max(1) as f32;
        let fov_deg = self.fov_deg;
        let density = self.density;
        let camera_distance = self.distance;

        // Projector used by every scene element: NDC position, a
        // depth-cued brightness multiplier (nearer geometry draws
        // brighter), and the clip-space depth written to the depth
        // attachment (pass-through today; world-space panes test against
        // it from slice 2 on).
        let mut project = move |p: Vec3| {
            camera
                .project_depth(p, aspect, fov_deg)
                .map(|(ndc, z)| (ndc, depth_alpha(z, camera_distance), ndc_depth(z)))
        };

        let star_alpha = self.mood.starfield_alpha();
        for (idx, star) in self
            .starfield
            .iter()
            .enumerate()
            .step_by(self.mood.starfield_stride())
        {
            if let Some((p, cue, z)) = project(*star) {
                let s = 0.0045 * density;
                let alpha = star_alpha * cue.min(1.0) * star_twinkle(anim_ms, idx);
                frame.add_quad_ndc(p.x, p.y, s, [0.35, 0.36, 0.44, alpha], z);
            }
        }

        self.add_grid(&mut frame, &mut project);
        self.add_operator(&mut frame, &mut project, anim_ms);

        for host in &self.snapshot.hosts {
            let id = format!("host:{}", host.id);
            if let Some(pos) = self.layout_cache.get(&id).copied() {
                self.add_host(&mut frame, host, pos, &mut project, anim_ms);
            }
        }
        for agent in &self.snapshot.agents {
            if let Some(pos) = self.layout_cache.get(&agent.id).copied() {
                self.add_agent(&mut frame, agent, pos, &mut project, anim_ms);
            }
        }

        let layout = &self.layout_cache;
        for agent in &self.snapshot.agents {
            let Some(a_pos) = layout.get(&agent.id).copied() else {
                continue;
            };
            let host_id = format!("host:{}", agent.host_id);
            if let Some(parent_id) = agent.parent_id.as_ref().filter(|p| !p.is_empty()) {
                if let Some(p_pos) = layout.get(parent_id).copied() {
                    frame.add_line_projected(
                        &mut project,
                        p_pos,
                        a_pos,
                        relationship_color(&agent.relationship_kind, &agent.role).with_alpha(0.54),
                    );
                    continue;
                }
            }
            if let Some(h_pos) = layout.get(&host_id).copied() {
                frame.add_line_projected(
                    &mut project,
                    h_pos,
                    a_pos,
                    role_color(&agent.role).with_alpha(0.42),
                );
            }
        }
        for host in &self.snapshot.hosts {
            let id = format!("host:{}", host.id);
            if let Some(pos) = layout.get(&id).copied() {
                frame.add_line_projected(&mut project, Vec3::ZERO, pos, C_BLUE.with_alpha(0.26));
            }
        }

        self.particles.retain(|particle| {
            let t = ((time_ms - particle.born_ms) as f32 / particle.ttl_ms as f32).clamp(0.0, 1.0);
            if t >= 1.0 {
                return false;
            }
            let lifted =
                particle.start.lerp(particle.end, t) + Vec3::new(0.0, (t * PI).sin() * 0.6, 0.0);
            if let Some((p, cue, z)) = project(lifted) {
                let size = (0.026 * (1.0 - t) + 0.006) * density;
                frame.add_quad_ndc(
                    p.x,
                    p.y,
                    size,
                    particle
                        .color
                        .with_alpha((0.88 * (1.0 - t) * cue).min(1.0))
                        .into(),
                    z,
                );
            }
            true
        });

        // Phase C slice 5, behind ?station_panes=on: the selected agent's
        // focus panel as a world pane beside its node — the shared focus
        // rows (focus_rows, the screen panel's exact content), a tokens
        // meter, and action pills whose projected rects the HUD pass
        // adopts as hit zones. Wide viewports only: under 820 CSS px the
        // compact screen surface is the better presentation, so the
        // scene stays out of its way and the HUD keeps its panel (it
        // yields only to an actually registered pane target). Hosts and
        // system nodes keep their screen panels until their panes
        // migrate in a later slice.
        if self.panes_enabled && self.css_width() >= 820.0 {
            if let Some(agent) = self
                .selected_id
                .as_ref()
                .and_then(|id| self.snapshot.agents.iter().find(|a| &a.id == id))
            {
                if let Some(pos) = self.layout_cache.get(&agent.id).copied() {
                    self.add_agent_focus_pane(
                        &mut frame,
                        agent,
                        pos,
                        cam_right,
                        cam_up,
                        &mut project,
                    );
                }
            }
        }

        self.frame = frame;
    }

    /// The selected agent's focus panel as a world pane (Phase C slice
    /// 5): card body, title + subtitle, the shared focus rows, and
    /// action pills (steer + session ops + approve/deny) laid onto the
    /// card, each pill's projected screen rect registered in
    /// `frame.pane_zones`. Emits nothing without a baked text atlas — a
    /// blank card must not replace the screen panel, and the HUD only
    /// yields when this agent's pane target actually exists.
    pub(crate) fn add_agent_focus_pane(
        &self,
        frame: &mut GpuFrame,
        agent: &StationAgent,
        pos: Vec3,
        right: Vec3,
        up: Vec3,
        project: &mut impl FnMut(Vec3) -> Option<(Vec2, f32, f32)>,
    ) {
        let Some(atlas) = self.text_atlas.as_ref() else {
            return;
        };
        let content = crate::focus_rows::agent_focus_content(
            agent,
            self.snapshot.hosts.first().map(|h| h.id.as_str()),
            epoch_seconds_now(),
        );
        // Action pills: a steer composer opener unless the session ops
        // already advertise a steer, then the shared content's pills.
        // Colors come from the same CSS palette the screen pills use.
        let mut pills: Vec<(&str, Color, HitAction)> = Vec::new();
        if !content.pills.iter().any(|p| p.label == "steer") {
            pills.push(("steer", C_BLUE, HitAction::Composer { op: "open-send" }));
        }
        for pill in &content.pills {
            pills.push((pill.label, css_color(pill.color_css), pill.action.clone()));
        }
        let pill_rows = pill_row_count(atlas, pills.iter().map(|p| p.0));
        let half_h = agent_pane_half_h(content.rows.len(), pill_rows, content.approval.is_some());
        // Readability floor (slice 6): the card keeps its intrinsic world
        // size up close but grows with camera distance so its projection
        // never drops below a legible width. The wrap/row arithmetic is
        // scale-invariant (all dims scale together), so only the emitted
        // geometry carries `s`.
        let Some((_, _, clip_depth)) = project(pos) else {
            return;
        };
        let px_per_world = (self.height as f32 * 0.5)
            / ((self.fov_deg.to_radians() * 0.5).tan() * view_z_from_clip_depth(clip_depth))
            / self.dpr as f32;
        let s = (PANE_MIN_CSS_W / (PANE_HALF_W * 2.0 * px_per_world)).clamp(1.0, 2.6);
        let (half_w, half_h) = (PANE_HALF_W * s, half_h * s);
        // Clear of the node up-right, scaling with the card so a tall
        // panel doesn't swallow its own anchor.
        let anchor = pos + right * (half_w + 0.30) + up * (half_h * 0.55);
        if !crate::panes::add_world_pane(
            frame,
            project,
            right,
            up,
            anchor,
            half_w,
            half_h,
            Color::rgb(16, 18, 32).with_alpha(0.86),
            0.0,
        ) {
            return;
        }
        // Register the card for raycast picking (slice 4,
        // input::pick_pane) — click-solid, matching its per-pixel
        // occlusion of the scene behind it.
        frame.pane_targets.push(crate::panes::PaneTarget {
            id: agent.id.clone(),
            anchor,
            right,
            up,
            half_w,
            half_h,
        });
        // Leader line: ties the card to its node (the 2D HUD's
        // thumbnail-anchor precedent), colored by the agent's phase.
        frame.add_line_projected(
            project,
            pos,
            anchor - right * half_w - up * (half_h * 0.35),
            phase_color(&agent.phase).with_alpha(0.38),
        );

        let inner_w = (PANE_HALF_W - PANE_MARGIN) * 2.0 * s;
        let top_left =
            anchor - right * (half_w - PANE_MARGIN * s) + up * (half_h - PANE_MARGIN * s);
        crate::text_atlas::add_text_world(
            frame,
            atlas,
            project,
            right,
            up,
            top_left,
            PANE_TITLE_H * s,
            &atlas.fit_to_width(&agent.id, PANE_TITLE_H * s, inner_w),
            C_TEXT,
        );
        let mut cursor = top_left - up * (PANE_TITLE_H * 1.3 * s);
        crate::text_atlas::add_text_world(
            frame,
            atlas,
            project,
            right,
            up,
            cursor,
            PANE_ROW_H * s,
            &atlas.fit_to_width(&content.subtitle, PANE_ROW_H * s, inner_w),
            C_SUBTEXT0,
        );
        cursor = cursor - up * (PANE_ROW_H * 1.25 * s);

        for row in &content.rows {
            pane_focus_row(frame, atlas, project, right, up, &mut cursor, row, s);
        }

        self.pane_pill_rows(frame, atlas, project, right, up, &mut cursor, &pills, s);

        if let Some(appr) = &content.approval {
            pane_focus_row(frame, atlas, project, right, up, &mut cursor, &appr.row, s);
            let decide = |decision: &'static str| HitAction::Approval {
                host_id: appr.host_id.clone(),
                approval_id: appr.approval_id.clone(),
                decision,
            };
            let pills = [
                ("approve", C_GREEN, decide("approve")),
                ("deny", C_RED, decide("deny")),
            ];
            self.pane_pill_rows(frame, atlas, project, right, up, &mut cursor, &pills, s);
        }
    }

    /// Pills on a pane, wrapping to as many rows as the labels need (the
    /// same pen walk `pill_row_count` sizes the card with): background
    /// quad, label, and the projected hit rect per pill. Unlike the
    /// screen panel, nothing is dropped — every advertised action stays
    /// reachable in the scene.
    #[allow(clippy::too_many_arguments)]
    fn pane_pill_rows(
        &self,
        frame: &mut GpuFrame,
        atlas: &crate::text_atlas::TextAtlas,
        project: &mut impl FnMut(Vec3) -> Option<(Vec2, f32, f32)>,
        right: Vec3,
        up: Vec3,
        cursor: &mut Vec3,
        pills: &[(&str, Color, HitAction)],
        s: f32,
    ) {
        if pills.is_empty() {
            return;
        }
        let row_h = PANE_ROW_H * s;
        let inner_w = (PANE_HALF_W - PANE_MARGIN) * 2.0 * s;
        let ph = row_h * 1.35;
        let mut pen = 0.0f32;
        for (label, color, action) in pills {
            let text_w = atlas.measure_world(label, row_h);
            let pw = text_w + row_h * 0.9;
            if pen > 0.0 && pen + pw > inner_w {
                *cursor = *cursor - up * (row_h * 1.6);
                pen = 0.0;
            }
            let center = *cursor + right * (pen + pw * 0.5) - up * (ph * 0.5);
            crate::panes::add_world_pane(
                frame,
                project,
                right,
                up,
                center,
                pw * 0.5,
                ph * 0.5,
                color.with_alpha(0.2),
                crate::panes::PANE_LAYER1_BIAS,
            );
            crate::text_atlas::add_text_world(
                frame,
                atlas,
                project,
                right,
                up,
                *cursor + right * (pen + (pw - text_w) * 0.5) - up * ((ph - row_h) * 0.5),
                row_h,
                label,
                *color,
            );
            self.push_pane_zone(
                frame,
                project,
                right,
                up,
                center,
                pw * 0.5,
                ph * 0.5,
                action.clone(),
            );
            pen += pw + row_h * 0.4;
        }
        *cursor = *cursor - up * (row_h * 1.6);
    }

    /// Project a pill's world rect to its CSS-px bounding box and
    /// register it (with the click action) in `frame.pane_zones` for the
    /// HUD pass to adopt. A pill straddling the frustum edge is skipped
    /// — its card was emitted whole, so this only drops zones in
    /// degenerate views.
    #[allow(clippy::too_many_arguments)]
    fn push_pane_zone(
        &self,
        frame: &mut GpuFrame,
        project: &mut impl FnMut(Vec3) -> Option<(Vec2, f32, f32)>,
        right: Vec3,
        up: Vec3,
        center: Vec3,
        half_w: f32,
        half_h: f32,
        action: HitAction,
    ) {
        let corners = [
            center - right * half_w - up * half_h,
            center + right * half_w - up * half_h,
            center + right * half_w + up * half_h,
            center - right * half_w + up * half_h,
        ];
        let (mut min_x, mut min_y) = (f32::MAX, f32::MAX);
        let (mut max_x, mut max_y) = (f32::MIN, f32::MIN);
        for corner in corners {
            let Some((ndc, _, _)) = project(corner) else {
                return;
            };
            let px = ndc_to_screen([ndc.x, ndc.y], self.width, self.height);
            min_x = min_x.min(px.x);
            min_y = min_y.min(px.y);
            max_x = max_x.max(px.x);
            max_y = max_y.max(px.y);
        }
        let dpr = self.dpr as f32;
        frame.pane_zones.push(crate::panes::PaneZone {
            x: min_x / dpr,
            y: min_y / dpr,
            w: (max_x - min_x) / dpr,
            h: (max_y - min_y) / dpr,
            action,
        });
    }

    /// CSS-px bounding rects of this frame's world panes (empty when
    /// none): the HUD uses them to keep 2D chrome from painting over an
    /// in-scene panel (the HUD canvas sits above the scene canvas), and
    /// `debug_json` exports them as `paneRects`. A pane with any corner
    /// culled reports no rect, matching the draw.
    pub(crate) fn pane_css_rects(&self) -> Vec<(String, f32, f32, f32, f32)> {
        if self.frame.pane_targets.is_empty() {
            return Vec::new();
        }
        let camera = self.camera();
        let aspect = self.width as f32 / self.height.max(1) as f32;
        let dpr = self.dpr as f32;
        self.frame
            .pane_targets
            .iter()
            .filter_map(|target| {
                let mut min = (f32::MAX, f32::MAX);
                let mut max = (f32::MIN, f32::MIN);
                for corner in [
                    target.anchor - target.right * target.half_w - target.up * target.half_h,
                    target.anchor + target.right * target.half_w - target.up * target.half_h,
                    target.anchor + target.right * target.half_w + target.up * target.half_h,
                    target.anchor - target.right * target.half_w + target.up * target.half_h,
                ] {
                    let (ndc, _z) = camera.project_depth(corner, aspect, self.fov_deg)?;
                    let p = ndc_to_screen([ndc.x, ndc.y], self.width, self.height);
                    min = (min.0.min(p.x), min.1.min(p.y));
                    max = (max.0.max(p.x), max.1.max(p.y));
                }
                Some((
                    target.id.clone(),
                    min.0 / dpr,
                    min.1 / dpr,
                    (max.0 - min.0) / dpr,
                    (max.1 - min.1) / dpr,
                ))
            })
            .collect()
    }

    pub(crate) fn add_grid(
        &self,
        frame: &mut GpuFrame,
        project: &mut impl FnMut(Vec3) -> Option<(Vec2, f32, f32)>,
    ) {
        let grid = 9;
        for i in -grid..=grid {
            let v = i as f32;
            let alpha = if i == 0 { 0.33 } else { 0.16 };
            frame.add_line_projected(
                project,
                Vec3::new(-9.0, -1.8, v),
                Vec3::new(9.0, -1.8, v),
                C_SURFACE0.with_alpha(alpha),
            );
            frame.add_line_projected(
                project,
                Vec3::new(v, -1.8, -9.0),
                Vec3::new(v, -1.8, 9.0),
                C_SURFACE0.with_alpha(alpha),
            );
        }
    }

    pub(crate) fn add_operator(
        &self,
        frame: &mut GpuFrame,
        project: &mut impl FnMut(Vec3) -> Option<(Vec2, f32, f32)>,
        time_ms: f64,
    ) {
        let pos = self.layout_cache.get("op").copied().unwrap_or(Vec3::ZERO);
        let spin = time_ms as f32 * 0.00032 * self.motion;
        let glow = self.mood.glow();
        frame.add_wire_octa(project, pos, 0.48, spin, C_BLUE.with_alpha(0.95));
        // The operator core is the scene's anchor: give its inner ring the
        // cheap two-pass glow (thick faint quad under a thin bright line).
        frame.add_glow_ring(
            project,
            pos,
            0.82,
            C_SAPPHIRE.with_alpha(0.55 * glow),
            Plane::XZ,
            GLOW_WIDTH,
        );
        frame.add_ring(
            project,
            pos,
            1.18,
            C_BLUE.with_alpha(0.18 * glow),
            Plane::XZ,
        );
        if self.selected_id.as_deref() == Some("op") {
            self.add_selection_halo(frame, project, pos, 1.32);
        }
        if let Some((p, _, _)) = project(pos) {
            frame.projected_nodes.push(ProjectedNode::new(
                "op",
                NodeKind::Operator,
                p,
                18.0 * self.density,
            ));
        }
    }

    /// Selection halo: a glowing ring around whichever node is selected.
    /// Drawn from `selected_id` every frame, so `select_by_id(None)` (or
    /// Escape / close) clears it on the next present.
    pub(crate) fn add_selection_halo(
        &self,
        frame: &mut GpuFrame,
        project: &mut impl FnMut(Vec3) -> Option<(Vec2, f32, f32)>,
        pos: Vec3,
        radius: f32,
    ) {
        frame.add_glow_ring(
            project,
            pos,
            radius,
            C_BLUE.with_alpha(0.88),
            Plane::XY,
            GLOW_WIDTH * 1.4,
        );
    }

    pub(crate) fn add_host(
        &self,
        frame: &mut GpuFrame,
        host: &StationHost,
        pos: Vec3,
        project: &mut impl FnMut(Vec3) -> Option<(Vec2, f32, f32)>,
        time_ms: f64,
    ) {
        let id = format!("host:{}", host.id);
        let spin = time_ms as f32 * 0.00011 * self.motion + stable_angle(&host.id);
        frame.add_wire_hex(
            project,
            pos,
            0.58,
            0.28,
            spin,
            C_PEACH.with_alpha(if host.connected { 0.9 } else { 0.38 }),
        );
        frame.add_ring(
            project,
            pos + Vec3::new(0.0, -0.17, 0.0),
            0.82 + (time_ms as f32 * 0.003).sin() * 0.035 * self.mood.pulse(),
            C_PEACH.with_alpha(0.28 * self.mood.glow()),
            Plane::XZ,
        );
        if self.selected_id.as_deref() == Some(&id) {
            self.add_selection_halo(frame, project, pos, 0.94);
        }
        if let Some((p, _, _)) = project(pos) {
            frame.projected_nodes.push(ProjectedNode::new(
                &id,
                NodeKind::Host,
                p,
                21.0 * self.density,
            ));
        }
    }

    pub(crate) fn add_agent(
        &self,
        frame: &mut GpuFrame,
        agent: &StationAgent,
        pos: Vec3,
        project: &mut impl FnMut(Vec3) -> Option<(Vec2, f32, f32)>,
        time_ms: f64,
    ) {
        let role = role_color(&agent.role);
        let phase = phase_color(&agent.phase);
        let spin = time_ms as f32 * 0.0005 * self.motion + stable_angle(&agent.id);
        // Recent (closed-window) sessions read as archive: dim body, no
        // pressure/phase rings — the constellation keeps its focus on what
        // is actually running.
        let body_alpha = if agent.recent { 0.42 } else { 0.95 };
        match agent.role.as_str() {
            "orchestrator" => {
                frame.add_wire_octa(project, pos, 0.34, spin, role.with_alpha(body_alpha + 0.01))
            }
            "sub-agent" => {
                frame.add_wire_tetra(project, pos, 0.31, spin, role.with_alpha(body_alpha))
            }
            _ => frame.add_wire_icosa(project, pos, 0.31, spin, role.with_alpha(body_alpha)),
        }
        if !agent.recent {
            let pct = if agent.token_cap > 0.0 {
                (agent.tokens / agent.token_cap).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let budget = if pct < 0.5 {
                C_GREEN
            } else if pct < 0.85 {
                C_YELLOW
            } else {
                C_RED
            };
            frame.add_ring(project, pos, 0.56, budget.with_alpha(0.66), Plane::XY);
            frame.add_ring(project, pos, 0.38, phase.with_alpha(0.2), Plane::YZ);
        }
        let goal_status = agent.goal_status.trim();
        if !goal_status.is_empty() {
            // Goal state as a thin band between the budget ring (0.56) and
            // the running pulse (0.72), tinted like the focus-panel goal row.
            frame.add_ring(
                project,
                pos,
                0.64,
                goal_status_color(goal_status).with_alpha(if agent.recent { 0.3 } else { 0.5 }),
                Plane::XY,
            );
        }
        if agent.status == "in_progress" || agent.phase == "running" {
            frame.add_ring(
                project,
                pos,
                0.72 + (time_ms as f32 * 0.004).sin() * 0.05 * self.mood.pulse(),
                C_TEAL.with_alpha(0.22 * self.mood.glow()),
                Plane::XY,
            );
        }
        if agent.needs_approval {
            // Approval requests must read from across the room: glow pass.
            frame.add_glow_ring(
                project,
                pos,
                0.84 + (time_ms as f32 * 0.006).sin() * 0.07 * self.mood.pulse(),
                C_YELLOW.with_alpha(0.58),
                Plane::XY,
                GLOW_WIDTH,
            );
        }
        if self.selected_id.as_deref() == Some(&agent.id) {
            self.add_selection_halo(frame, project, pos, 0.96);
        }
        if let Some(parent_id) = agent.parent_id.as_ref().filter(|s| !s.is_empty()) {
            if let Some(parent) = self.layout_cache.get(parent_id).copied() {
                frame.add_line_projected(
                    project,
                    parent,
                    pos,
                    relationship_color(&agent.relationship_kind, &agent.role).with_alpha(0.5),
                );
            }
        }
        if let Some((p, _, _)) = project(pos) {
            frame.projected_nodes.push(ProjectedNode::new(
                &agent.id,
                NodeKind::Agent,
                p,
                15.0 * self.density,
            ));
        }
    }

    /// Stroke the frame's projected line list into a 2D context: the scene
    /// canvas when WebGPU is off, or the HUD canvas (as an underlay) when the
    /// scene canvas was consumed by a failed WebGPU init. Consecutive
    /// same-color segments share one path, and the stroke style is only
    /// touched when the color changes.
    pub(crate) fn draw_scene_lines(&self, ctx: &CanvasRenderingContext2d) {
        ctx.set_fill_style_str("rgba(17,17,27,0.94)");
        ctx.fill_rect(0.0, 0.0, self.width as f64, self.height as f64);
        let mut current: Option<[f32; 4]> = None;
        let mut open = false;
        for pair in self.frame.line_vertices.chunks_exact(2) {
            if current != Some(pair[0].color) {
                if open {
                    ctx.stroke();
                }
                ctx.set_stroke_style_str(&css_rgba(pair[0].color));
                ctx.begin_path();
                current = Some(pair[0].color);
                open = true;
            }
            let a = ndc_to_screen(pair[0].pos, self.width, self.height);
            let b = ndc_to_screen(pair[1].pos, self.width, self.height);
            ctx.move_to(a.x as f64, a.y as f64);
            ctx.line_to(b.x as f64, b.y as f64);
        }
        if open {
            ctx.stroke();
        }
    }

    pub(crate) fn camera(&self) -> Camera {
        let parallax = Vec3::new(
            self.ar_x * self.ar_strength,
            self.ar_y * self.ar_strength * 0.5,
            0.0,
        );
        let cp = self.pitch.cos();
        let eye = Vec3::new(
            self.yaw.sin() * cp * self.distance,
            self.pitch.sin() * self.distance + 3.2,
            self.yaw.cos() * cp * self.distance,
        ) + parallax;
        Camera::look_at(eye, Vec3::new(0.0, 0.25, 0.0), Vec3::Y)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LayoutName {
    Orbital,
    Constellation,
}

impl LayoutName {
    pub(crate) fn from_str(s: &str) -> Self {
        match s {
            "constellation" => Self::Constellation,
            _ => Self::Orbital,
        }
    }

    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::Orbital => "orbital",
            Self::Constellation => "constellation",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mood {
    Cockpit,
    Calm,
}

impl Mood {
    pub(crate) fn from_str(s: &str) -> Self {
        match s {
            "calm" => Self::Calm,
            _ => Self::Cockpit,
        }
    }

    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::Cockpit => "cockpit",
            Self::Calm => "calm",
        }
    }

    /// Starfield quad alpha: calm dims the backdrop.
    pub(crate) fn starfield_alpha(self) -> f32 {
        match self {
            Self::Cockpit => 0.55,
            Self::Calm => 0.32,
        }
    }

    /// Starfield sampling stride: calm draws every other star.
    pub(crate) fn starfield_stride(self) -> usize {
        match self {
            Self::Cockpit => 1,
            Self::Calm => 2,
        }
    }

    /// Amplitude scale for breathing/pulse animations.
    pub(crate) fn pulse(self) -> f32 {
        match self {
            Self::Cockpit => 1.0,
            Self::Calm => 0.45,
        }
    }

    /// Alpha scale for decorative (non-semantic) glow rings.
    pub(crate) fn glow(self) -> f32 {
        match self {
            Self::Cockpit => 1.0,
            Self::Calm => 0.65,
        }
    }

    /// Alpha scale for the HUD glass chrome (borders, sheen, corner glow);
    /// calm dims the accents along with the scene.
    pub(crate) fn glass(self) -> f32 {
        match self {
            Self::Cockpit => 1.0,
            Self::Calm => 0.6,
        }
    }

    /// Radial vignette color stops; calm is softer and less saturated.
    pub(crate) fn vignette_stops(self) -> [(f64, &'static str); 3] {
        match self {
            Self::Cockpit => [
                (0.0, "rgba(30,30,46,0.04)"),
                (0.75, "rgba(17,17,27,0.16)"),
                (1.0, "rgba(4,4,9,0.48)"),
            ],
            Self::Calm => [
                (0.0, "rgba(30,30,46,0.03)"),
                (0.75, "rgba(17,17,27,0.10)"),
                (1.0, "rgba(4,4,9,0.36)"),
            ],
        }
    }
}

pub(crate) struct Particle {
    pub(crate) start: Vec3,
    pub(crate) end: Vec3,
    pub(crate) born_ms: f64,
    pub(crate) ttl_ms: f64,
    pub(crate) color: Color,
}

#[derive(Clone)]
pub(crate) struct ProjectedNode {
    pub(crate) id: String,
    pub(crate) kind: NodeKind,
    pub(crate) ndc: Vec2,
    pub(crate) radius: f32,
}

impl ProjectedNode {
    pub(crate) fn new(id: &str, kind: NodeKind, ndc: Vec2, radius: f32) -> Self {
        Self {
            id: id.to_string(),
            kind,
            ndc,
            radius,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum NodeKind {
    Operator,
    Host,
    Agent,
}

#[derive(Clone, Copy)]
pub(crate) enum Plane {
    XY,
    XZ,
    YZ,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Vec2 {
    pub(crate) x: f32,
    pub(crate) y: f32,
}

impl Vec2 {
    pub(crate) fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Vec3 {
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) z: f32,
}

impl Vec3 {
    pub(crate) const ZERO: Self = Self {
        x: 0.0,
        y: 0.0,
        z: 0.0,
    };
    pub(crate) const Y: Self = Self {
        x: 0.0,
        y: 1.0,
        z: 0.0,
    };

    pub(crate) fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }

    pub(crate) fn dot(self, rhs: Self) -> f32 {
        self.x * rhs.x + self.y * rhs.y + self.z * rhs.z
    }

    pub(crate) fn cross(self, rhs: Self) -> Self {
        Self {
            x: self.y * rhs.z - self.z * rhs.y,
            y: self.z * rhs.x - self.x * rhs.z,
            z: self.x * rhs.y - self.y * rhs.x,
        }
    }

    pub(crate) fn len(self) -> f32 {
        self.dot(self).sqrt()
    }

    pub(crate) fn normalized(self) -> Self {
        let len = self.len();
        if len < 0.0001 {
            Self::ZERO
        } else {
            self * (1.0 / len)
        }
    }

    pub(crate) fn lerp(self, rhs: Self, t: f32) -> Self {
        self * (1.0 - t) + rhs * t
    }
}

impl std::ops::Add for Vec3 {
    type Output = Self;
    fn add(self, rhs: Self) -> Self::Output {
        Self::new(self.x + rhs.x, self.y + rhs.y, self.z + rhs.z)
    }
}

impl std::ops::Sub for Vec3 {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self::Output {
        Self::new(self.x - rhs.x, self.y - rhs.y, self.z - rhs.z)
    }
}

impl std::ops::Mul<f32> for Vec3 {
    type Output = Self;
    fn mul(self, rhs: f32) -> Self::Output {
        Self::new(self.x * rhs, self.y * rhs, self.z * rhs)
    }
}

pub(crate) struct Camera {
    pub(crate) eye: Vec3,
    pub(crate) right: Vec3,
    pub(crate) up: Vec3,
    pub(crate) forward: Vec3,
}

impl Camera {
    pub(crate) fn look_at(eye: Vec3, target: Vec3, world_up: Vec3) -> Self {
        let forward = (target - eye).normalized();
        let right = forward.cross(world_up).normalized();
        let up = right.cross(forward).normalized();
        Self {
            eye,
            right,
            up,
            forward,
        }
    }

    /// Project a world position to NDC, also returning the view-space depth
    /// so callers can depth-cue brightness. Culls behind-camera and
    /// far-outside-frustum points.
    pub(crate) fn project_depth(
        &self,
        world: Vec3,
        aspect: f32,
        fov_deg: f32,
    ) -> Option<(Vec2, f32)> {
        let p = world - self.eye;
        let z = p.dot(self.forward);
        if z <= 0.12 {
            return None;
        }
        let x = p.dot(self.right);
        let y = p.dot(self.up);
        let f = 1.0 / (fov_deg.to_radians() * 0.5).tan();
        let ndc_x = (x * f / aspect) / z;
        let ndc_y = (y * f) / z;
        if ndc_x.abs() > 2.2 || ndc_y.abs() > 2.2 {
            return None;
        }
        Some((Vec2::new(ndc_x, ndc_y), z))
    }

    /// Inverse of `project_depth`: the world-space ray from the eye
    /// through an NDC point. The direction is normalized; every point
    /// `eye + dir * t` (t > 0) projects back onto `ndc`.
    pub(crate) fn ray_through(&self, ndc: Vec2, aspect: f32, fov_deg: f32) -> (Vec3, Vec3) {
        let f = 1.0 / (fov_deg.to_radians() * 0.5).tan();
        let dir = self.forward + self.right * (ndc.x * aspect / f) + self.up * (ndc.y / f);
        (self.eye, dir.normalized())
    }
}

/// Half-width (in NDC) of the faint thick pass behind glowing lines.
pub(crate) const GLOW_WIDTH: f32 = 0.007;

/// Focus-pane geometry (Phase C), world units: pane half-width, inner
/// text margin, title/row glyph-cell heights, and the label column
/// where row values start (mirroring the screen panel's 96px column;
/// wide enough that the longest labels — "worktree", "approval", 8
/// chars at ~0.047/glyph — fit un-ellipsized). The half-HEIGHT is
/// content-driven — `agent_pane_half_h`. Glyph heights are sized so
/// text draws near the atlas's baked size at typical camera distances
/// (see `text_atlas` on sampling quality).
const PANE_HALF_W: f32 = 0.95;
const PANE_MARGIN: f32 = 0.07;
const PANE_TITLE_H: f32 = 0.14;
const PANE_ROW_H: f32 = 0.105;
const PANE_LABEL_COL: f32 = 0.44;
/// Readability floor (slice 6): minimum projected card width in CSS px —
/// the pane scales up with camera distance until it holds this width
/// (clamped, so a pathological distance can't blow it up unbounded).
const PANE_MIN_CSS_W: f32 = 280.0;

/// Inverse of `ndc_depth`: recover view-space z from the clip depth the
/// projector returned (clamped away from the 1.0 asymptote).
pub(crate) fn view_z_from_clip_depth(d: f32) -> f32 {
    (1.0 / (1.0 - d.clamp(0.0, 0.999)) - 1.0).max(0.001)
}

/// Content-driven pane half-height: title block, subtitle row, `rows`
/// content rows, the tokens-meter band, `pill_rows` wrapped action-pill
/// rows, and — when actionable — the approval row plus its own pill row
/// (approve + deny always fit one row at the pane's inner width). Kept
/// in lockstep with the layout walk in
/// `add_agent_focus_pane`/`pane_focus_row`/`pane_pill_rows`.
pub(crate) fn agent_pane_half_h(rows: usize, pill_rows: usize, approval: bool) -> f32 {
    let pitch = PANE_ROW_H * 1.25;
    let mut content_h = PANE_TITLE_H * 1.3            // title
        + (rows as f32 + 1.0) * pitch                 // subtitle + rows
        + PANE_ROW_H * 0.5                            // tokens meter band
        + pill_rows as f32 * (PANE_ROW_H * 1.6); //      wrapped pill rows
    if approval {
        content_h += pitch + PANE_ROW_H * 1.6;
    }
    (content_h + PANE_MARGIN * 2.0) * 0.5
}

/// Number of wrapped rows the pill labels occupy at the pane's inner
/// width — the same pen walk `pane_pill_rows` renders with, so sizing
/// and layout cannot disagree.
fn pill_row_count<'a>(
    atlas: &crate::text_atlas::TextAtlas,
    labels: impl Iterator<Item = &'a str>,
) -> usize {
    let inner_w = (PANE_HALF_W - PANE_MARGIN) * 2.0;
    let mut rows = 0usize;
    let mut pen = 0.0f32;
    for label in labels {
        let pw = atlas.measure_world(label, PANE_ROW_H) + PANE_ROW_H * 0.9;
        if rows == 0 || (pen > 0.0 && pen + pw > inner_w) {
            rows += 1;
            if pen > 0.0 {
                pen = 0.0;
            }
        }
        pen += pw + PANE_ROW_H * 0.4;
    }
    rows
}

/// One shared focus row on a pane: colored label column, value text
/// beside it (the world-space counterpart of `hud::focus_row`), plus
/// the meter band under a row that carries one. Advances `cursor` past
/// the row (and band).
#[allow(clippy::too_many_arguments)]
fn pane_focus_row(
    frame: &mut GpuFrame,
    atlas: &crate::text_atlas::TextAtlas,
    project: &mut impl FnMut(Vec3) -> Option<(Vec2, f32, f32)>,
    right: Vec3,
    up: Vec3,
    cursor: &mut Vec3,
    row: &crate::focus_rows::AgentFocusRow,
    s: f32,
) {
    let row_h = PANE_ROW_H * s;
    let label_col = PANE_LABEL_COL * s;
    let inner_w = (PANE_HALF_W - PANE_MARGIN) * 2.0 * s;
    crate::text_atlas::add_text_world(
        frame,
        atlas,
        project,
        right,
        up,
        *cursor,
        row_h,
        &atlas.fit_to_width(row.label, row_h, label_col - 0.03 * s),
        css_color(row.color_css),
    );
    crate::text_atlas::add_text_world(
        frame,
        atlas,
        project,
        right,
        up,
        *cursor + right * label_col,
        row_h,
        &atlas.fit_to_width(&row.value, row_h, inner_w - label_col),
        C_TEXT,
    );
    *cursor = *cursor - up * (row_h * 1.25);
    if let Some(pct) = row.meter {
        let track_w = inner_w - label_col;
        let mh = row_h * 0.18;
        let track_center = *cursor + right * (label_col + track_w * 0.5) - up * (mh * 0.5);
        crate::panes::add_world_pane(
            frame,
            project,
            right,
            up,
            track_center,
            track_w * 0.5,
            mh * 0.5,
            C_SURFACE0.with_alpha(0.92),
            crate::panes::PANE_LAYER1_BIAS,
        );
        let frac = pct.clamp(0.0, 1.0);
        if frac > 0.001 {
            let fill_w = track_w * frac;
            let fill_center = *cursor + right * (label_col + fill_w * 0.5) - up * (mh * 0.5);
            crate::panes::add_world_pane(
                frame,
                project,
                right,
                up,
                fill_center,
                fill_w * 0.5,
                mh * 0.5,
                css_color(pressure_color(pct)).with_alpha(0.95),
                crate::panes::PANE_LAYER2_BIAS,
            );
        }
        *cursor = *cursor - up * (row_h * 0.5);
    }
}

/// Depth-cued brightness: geometry nearer than the orbit center draws a
/// little brighter, farther a little dimmer. `z` is view-space depth and
/// `camera_distance` the orbit radius, so the scene center sits at 1.0.
pub(crate) fn depth_alpha(z: f32, camera_distance: f32) -> f32 {
    (1.0 + (camera_distance - z) * 0.04).clamp(0.6, 1.18)
}

/// View-space depth → clip-space z in [0, 1) for the depth attachment.
/// Asymptotic rather than near/far-planed: monotonic in view depth, can
/// never leave the clip range (out-of-range z would silently clip the
/// vertex), and needs no far-plane constant — ordering is all the depth
/// test needs. Revisit precision only if pane occlusion ever demands it.
pub(crate) fn ndc_depth(view_z: f32) -> f32 {
    1.0 - 1.0 / (1.0 + view_z.max(0.0))
}

/// Gentle per-star twinkle, frozen (1.0) whenever ambient motion is off —
/// `anim_ms` is already zeroed at motion 0, so a parked scene stays still.
pub(crate) fn star_twinkle(anim_ms: f64, idx: usize) -> f32 {
    if anim_ms == 0.0 {
        return 1.0;
    }
    0.84 + 0.16 * (anim_ms as f32 * 0.0011 + idx as f32 * 2.39).sin()
}

pub(crate) fn rotate_y(v: Vec3, a: f32) -> Vec3 {
    let (s, c) = a.sin_cos();
    Vec3::new(v.x * c + v.z * s, v.y, -v.x * s + v.z * c)
}

pub(crate) fn rotate_x(v: Vec3, a: f32) -> Vec3 {
    let (s, c) = a.sin_cos();
    Vec3::new(v.x, v.y * c - v.z * s, v.y * s + v.z * c)
}

pub(crate) fn ndc_to_screen(pos: [f32; 2], width: u32, height: u32) -> Vec2 {
    Vec2::new(
        (pos[0] * 0.5 + 0.5) * width as f32,
        (0.5 - pos[1] * 0.5) * height as f32,
    )
}

/// Inverse of `ndc_to_screen`: device pixels back to NDC.
pub(crate) fn screen_to_ndc(px: f32, py: f32, width: u32, height: u32) -> Vec2 {
    Vec2::new(
        px / width.max(1) as f32 * 2.0 - 1.0,
        1.0 - py / height.max(1) as f32 * 2.0,
    )
}

/// World position per node id ("op", "host:<id>", agent ids) for the given
/// layout. Pure: depends only on the snapshot and layout, so callers cache
/// the result per (snapshot, layout) change.
pub(crate) fn layout_positions(
    snapshot: &StationSnapshot,
    layout: LayoutName,
) -> HashMap<String, Vec3> {
    let mut map = HashMap::new();
    map.insert("op".to_string(), Vec3::ZERO);
    let host_count = snapshot.hosts.len().max(1);
    for (i, host) in snapshot.hosts.iter().enumerate() {
        let t = i as f32 / host_count as f32;
        let pos = match layout {
            LayoutName::Orbital => {
                let angle = t * PI * 2.0 + PI * 0.08;
                let radius = 4.2 + (host_count as f32 * 0.18).min(1.3);
                Vec3::new(angle.cos() * radius, 0.0, angle.sin() * radius)
            }
            LayoutName::Constellation => {
                let spread = (host_count as f32 - 1.0).max(1.0);
                let x = (i as f32 - spread * 0.5) * 3.2;
                let z = -1.3 + (stable_unit(&host.id) - 0.5) * 2.3;
                Vec3::new(
                    x,
                    -0.05 + (stable_unit(&(host.id.clone() + "y")) - 0.5) * 0.8,
                    z,
                )
            }
        };
        map.insert(format!("host:{}", host.id), pos);
    }
    let mut by_host: HashMap<&str, Vec<&StationAgent>> = HashMap::new();
    for agent in &snapshot.agents {
        by_host
            .entry(agent.host_id.as_str())
            .or_default()
            .push(agent);
    }
    for host in &snapshot.hosts {
        let host_pos = map
            .get(&format!("host:{}", host.id))
            .copied()
            .unwrap_or(Vec3::ZERO);
        let agents = by_host.get(host.id.as_str()).cloned().unwrap_or_default();
        let count = agents.len().max(1);
        for (idx, agent) in agents.into_iter().enumerate() {
            let pos = match layout {
                LayoutName::Orbital => {
                    let angle = idx as f32 / count as f32 * PI * 2.0 + stable_angle(&agent.id);
                    let ring = if agent.role == "sub-agent" {
                        1.55
                    } else {
                        1.18
                    };
                    host_pos
                        + Vec3::new(
                            angle.cos() * ring,
                            0.55 + (idx % 3) as f32 * 0.28,
                            angle.sin() * ring * 0.72,
                        )
                }
                LayoutName::Constellation => {
                    let u = stable_unit(&agent.id);
                    let v = stable_unit(&(agent.id.clone() + "v"));
                    host_pos
                        + Vec3::new(
                            (u - 0.5) * 2.9,
                            0.7 + v * 1.8,
                            (stable_unit(&(agent.id.clone() + "z")) - 0.5) * 2.0,
                        )
                }
            };
            map.insert(agent.id.clone(), pos);
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::StationAgent;

    #[test]
    fn agent_pane_half_h_scales_with_rows_pills_and_approval() {
        let pitch = PANE_ROW_H * 1.25;
        let base = agent_pane_half_h(5, 1, false);
        // Three more rows grow the half-height by half of three pitches.
        let more = agent_pane_half_h(8, 1, false);
        assert!((more - base - 3.0 * pitch * 0.5).abs() < 1e-6);
        // A wrapped second pill row adds half a pill-row height.
        let wrapped = agent_pane_half_h(5, 2, false);
        assert!((wrapped - base - PANE_ROW_H * 1.6 * 0.5).abs() < 1e-6);
        // An actionable approval adds its row and a pill row.
        let approval = agent_pane_half_h(5, 1, true);
        assert!((approval - base - (pitch + PANE_ROW_H * 1.6) * 0.5).abs() < 1e-6);
        // Exact arithmetic for the base case, pinned against the layout
        // walk's constants (title 1.3, pitch 1.25, meter 0.5, pills 1.6).
        let content = PANE_TITLE_H * 1.3 + 6.0 * pitch + PANE_ROW_H * 0.5 + PANE_ROW_H * 1.6;
        assert!((base - (content + PANE_MARGIN * 2.0) * 0.5).abs() < 1e-6);
    }

    #[test]
    fn view_z_round_trips_ndc_depth() {
        for z in [0.2f32, 1.0, 4.0, 11.0, 40.0] {
            let recovered = view_z_from_clip_depth(ndc_depth(z));
            assert!(
                (recovered - z).abs() / z < 1e-3,
                "z {z} recovered as {recovered}"
            );
        }
        // The clamp keeps the asymptote and negatives finite and sane.
        assert!(view_z_from_clip_depth(1.0).is_finite());
        assert!(view_z_from_clip_depth(-0.5) > 0.0);
    }

    #[test]
    fn pill_row_count_wraps_like_the_renderer_pen_walk() {
        // Synthetic atlas ('a' advances 10px, 'b' 14px); at height h the
        // world advance is px * (h / CELL_H).
        let atlas = crate::text_atlas::test_atlas();
        assert_eq!(pill_row_count(&atlas, [].into_iter()), 0);
        assert_eq!(pill_row_count(&atlas, ["a"].into_iter()), 1);
        // A couple of short labels share one row; a long run of wide
        // labels must wrap. Monotonic rather than pen-exact: the sizing
        // walk and the renderer share the same arithmetic.
        let one = pill_row_count(&atlas, ["aa", "bb"].into_iter());
        let many = pill_row_count(&atlas, ["bbbbbbbb"; 12].into_iter());
        assert_eq!(one, 1);
        assert!(many > 1, "twelve wide labels must wrap ({many} rows)");
    }

    #[test]
    fn ndc_depth_is_monotonic_and_clip_safe() {
        // Strictly increasing in view depth: farther geometry writes a
        // larger depth value.
        let samples = [0.0, 0.12, 1.0, 4.0, 10.0, 100.0, 1e6];
        for pair in samples.windows(2) {
            assert!(ndc_depth(pair[0]) < ndc_depth(pair[1]));
        }
        // Never leaves [0, 1): an out-of-range z would clip the vertex.
        for z in samples {
            let d = ndc_depth(z);
            assert!((0.0..1.0).contains(&d), "z={z} mapped to {d}");
        }
        assert_eq!(ndc_depth(0.0), 0.0);
        // Degenerate negative depth (behind the camera; the projector
        // culls these before mapping) still stays in range.
        assert_eq!(ndc_depth(-3.0), 0.0);
    }

    fn snapshot() -> StationSnapshot {
        StationSnapshot {
            hosts: vec![
                StationHost {
                    id: "alpha".into(),
                    ..Default::default()
                },
                StationHost {
                    id: "beta".into(),
                    ..Default::default()
                },
            ],
            agents: vec![
                StationAgent {
                    id: "agent-1".into(),
                    host_id: "alpha".into(),
                    ..Default::default()
                },
                StationAgent {
                    id: "agent-2".into(),
                    host_id: "alpha".into(),
                    role: "sub-agent".into(),
                    ..Default::default()
                },
                StationAgent {
                    id: "agent-3".into(),
                    host_id: "beta".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        }
    }

    fn assert_same_positions(a: &HashMap<String, Vec3>, b: &HashMap<String, Vec3>) {
        assert_eq!(a.len(), b.len());
        for (key, pa) in a {
            let pb = b.get(key).unwrap_or_else(|| panic!("missing key {key}"));
            assert_eq!(
                (pa.x.to_bits(), pa.y.to_bits(), pa.z.to_bits()),
                (pb.x.to_bits(), pb.y.to_bits(), pb.z.to_bits()),
                "position differs for {key}"
            );
        }
    }

    #[test]
    fn layout_positions_is_deterministic() {
        let snapshot = snapshot();
        for layout in [LayoutName::Orbital, LayoutName::Constellation] {
            let a = layout_positions(&snapshot, layout);
            let b = layout_positions(&snapshot, layout);
            assert_same_positions(&a, &b);
        }
    }

    #[test]
    fn layout_positions_covers_every_node() {
        let snapshot = snapshot();
        let map = layout_positions(&snapshot, LayoutName::Orbital);
        assert!(map.contains_key("op"));
        assert!(map.contains_key("host:alpha"));
        assert!(map.contains_key("host:beta"));
        for agent in &snapshot.agents {
            assert!(map.contains_key(&agent.id), "missing {}", agent.id);
        }
        assert_eq!(map.len(), 1 + snapshot.hosts.len() + snapshot.agents.len());
    }

    #[test]
    fn layouts_actually_differ() {
        let snapshot = snapshot();
        let orbital = layout_positions(&snapshot, LayoutName::Orbital);
        let constellation = layout_positions(&snapshot, LayoutName::Constellation);
        let a = orbital.get("host:alpha").unwrap();
        let b = constellation.get("host:alpha").unwrap();
        assert!(
            (a.x - b.x).abs() > 1e-6 || (a.y - b.y).abs() > 1e-6 || (a.z - b.z).abs() > 1e-6,
            "orbital and constellation should place hosts differently"
        );
    }

    #[test]
    fn ray_through_inverts_projection() {
        let camera = Camera::look_at(
            Vec3::new(4.0, 3.0, 10.0),
            Vec3::new(0.0, 0.25, 0.0),
            Vec3::Y,
        );
        let (aspect, fov) = (16.0 / 9.0, 55.0);
        for world in [
            Vec3::ZERO,
            Vec3::new(1.3, -0.4, 2.0),
            Vec3::new(-2.0, 1.5, -1.0),
        ] {
            let (ndc, _z) = camera.project_depth(world, aspect, fov).unwrap();
            let (origin, dir) = camera.ray_through(ndc, aspect, fov);
            // The ray must pass (numerically) through the source point,
            // in front of the eye.
            let along = (world - origin).dot(dir);
            assert!(along > 0.0);
            let closest = origin + dir * along;
            let miss = (world - closest).len();
            assert!(miss < 1e-4, "ray misses its source point by {miss}");
        }
    }

    #[test]
    fn screen_to_ndc_round_trips_with_ndc_to_screen() {
        for (x, y) in [(0.0, 0.0), (100.0, 50.0), (199.0, 99.0), (37.5, 81.25)] {
            let ndc = screen_to_ndc(x, y, 200, 100);
            let back = ndc_to_screen([ndc.x, ndc.y], 200, 100);
            assert!((back.x - x).abs() < 1e-4 && (back.y - y).abs() < 1e-4);
        }
    }

    #[test]
    fn camera_projects_target_near_center_and_culls_behind() {
        let eye = Vec3::new(0.0, 0.0, 10.0);
        let camera = Camera::look_at(eye, Vec3::ZERO, Vec3::Y);
        let (center, z) = camera.project_depth(Vec3::ZERO, 16.0 / 9.0, 55.0).unwrap();
        assert!(center.x.abs() < 1e-5 && center.y.abs() < 1e-5);
        // View-space depth along the forward axis.
        assert!((z - 10.0).abs() < 1e-5);
        // A point behind the camera must be culled.
        assert!(camera
            .project_depth(Vec3::new(0.0, 0.0, 20.0), 16.0 / 9.0, 55.0)
            .is_none());
    }

    #[test]
    fn depth_alpha_brightens_near_and_dims_far() {
        let center = depth_alpha(11.0, 11.0);
        assert!((center - 1.0).abs() < 1e-6);
        assert!(depth_alpha(5.0, 11.0) > center);
        assert!(depth_alpha(17.0, 11.0) < center);
        // Extremes clamp instead of inverting or blowing out.
        assert_eq!(depth_alpha(0.2, 11.0), 1.18);
        assert_eq!(depth_alpha(40.0, 11.0), 0.6);
    }

    #[test]
    fn star_twinkle_freezes_without_motion_and_stays_subtle() {
        assert_eq!(star_twinkle(0.0, 7), 1.0);
        for idx in 0..32 {
            let t = star_twinkle(1234.5, idx);
            assert!((0.68..=1.0).contains(&t), "idx {idx} -> {t}");
        }
        // Different stars twinkle out of phase.
        assert_ne!(star_twinkle(1234.5, 0), star_twinkle(1234.5, 1));
    }

    #[test]
    fn vec3_math_basics() {
        let v = Vec3::new(3.0, 0.0, 4.0);
        assert_eq!(v.len(), 5.0);
        let n = v.normalized();
        assert!((n.len() - 1.0).abs() < 1e-6);
        assert_eq!(Vec3::ZERO.normalized().len(), 0.0);
        let lerped = Vec3::ZERO.lerp(Vec3::new(2.0, 2.0, 2.0), 0.5);
        assert_eq!((lerped.x, lerped.y, lerped.z), (1.0, 1.0, 1.0));
        let cross = Vec3::new(1.0, 0.0, 0.0).cross(Vec3::new(0.0, 1.0, 0.0));
        assert_eq!((cross.x, cross.y, cross.z), (0.0, 0.0, 1.0));
    }

    #[test]
    fn rotations_preserve_length() {
        let v = Vec3::new(1.0, 2.0, 3.0);
        assert!((rotate_y(v, 1.3).len() - v.len()).abs() < 1e-5);
        assert!((rotate_x(v, -0.7).len() - v.len()).abs() < 1e-5);
    }

    #[test]
    fn ndc_to_screen_maps_corners() {
        let top_left = ndc_to_screen([-1.0, 1.0], 200, 100);
        assert_eq!((top_left.x, top_left.y), (0.0, 0.0));
        let bottom_right = ndc_to_screen([1.0, -1.0], 200, 100);
        assert_eq!((bottom_right.x, bottom_right.y), (200.0, 100.0));
        let center = ndc_to_screen([0.0, 0.0], 200, 100);
        assert_eq!((center.x, center.y), (100.0, 50.0));
    }

    #[test]
    fn mood_parsing_and_factors() {
        assert_eq!(Mood::from_str("calm"), Mood::Calm);
        assert_eq!(Mood::from_str("anything"), Mood::Cockpit);
        assert!(Mood::Calm.starfield_alpha() < Mood::Cockpit.starfield_alpha());
        assert!(Mood::Calm.pulse() < Mood::Cockpit.pulse());
        assert!(Mood::Calm.glow() < Mood::Cockpit.glow());
        // Calm also dims the HUD glass chrome, not just the scene.
        assert!(Mood::Calm.glass() < Mood::Cockpit.glass());
        assert_eq!(Mood::Calm.starfield_stride(), 2);
        assert_eq!(
            LayoutName::from_str("constellation"),
            LayoutName::Constellation
        );
        assert_eq!(LayoutName::from_str("bogus"), LayoutName::Orbital);
    }
}
