//! Glitch-core frame processing engine for OBS.
//!
//! Handles two video paths:
//! - **filter_video** — async sources (PipeWire, webcam) provide raw CPU frames
//! - **video_render** — sync sources (display/window/game capture) use GPU staging

use glitch_core::{parser, eval, EvalContext, Rgb, Token};
use image::Rgba;
use rand::prelude::*;
use rayon::prelude::*;
use std::ffi::{c_char, CStr};
use std::ptr;
use std::time::Instant;

use obs_sys::*;

use crate::websocket::{self, Command, FilterSnapshot};

const RENDER_PROCESS_INTERVAL: u64 = 2;

// ─── GlitchFilter ────────────────────────────────────────────────

pub struct GlitchFilter {
    /// OBS source context (the filter instance)
    pub source: *mut obs_source_t,

    // ── Pixel buffers (reused across frames to avoid allocation) ──
    pub buffer: Vec<u8>,
    process_buf: Vec<u8>,

    // ── Glitch engine state (mirrors WASM GlitchEngine) ──
    pub expressions: Vec<(String, Vec<Token>)>,
    pub seed: u64,
    pub enabled: bool,
    pub frame_count: u64,

    // ── Pulse timer (auto-revert after duration) ──
    saved_expressions: Option<Vec<(String, Vec<Token>)>>,
    revert_at: Option<Instant>,

    // ── GPU resources for sync (video_render) path ──
    texrender: *mut gs_texrender_t,
    stagesurface: *mut gs_stagesurf_t,
    output_texture: *mut gs_texture_t,
    render_width: u32,
    render_height: u32,
    /// Set when filter_video fires — tells render() to passthrough (already processed).
    async_source: bool,
}

impl GlitchFilter {
    /// Create a new filter instance.
    pub unsafe fn new(source: *mut obs_source_t, settings: *mut obs_data_t) -> Self {
        let mut filter = Self {
            source,
            buffer: Vec::new(),
            process_buf: Vec::new(),
            expressions: Vec::new(),
            seed: 42,
            enabled: true,
            frame_count: 0,
            saved_expressions: None,
            revert_at: None,
            texrender: ptr::null_mut(),
            stagesurface: ptr::null_mut(),
            output_texture: ptr::null_mut(),
            render_width: 0,
            render_height: 0,
            async_source: false,
        };
        filter.update_settings(settings);
        filter
    }

    /// Read settings from OBS data object and re-parse expressions.
    pub unsafe fn update_settings(&mut self, settings: *mut obs_data_t) {
        if settings.is_null() {
            return;
        }

        // Expression
        let expr_ptr = obs_data_get_string(
            settings,
            b"expression\0".as_ptr() as *const c_char,
        );
        if !expr_ptr.is_null() {
            let expr_str = std::ffi::CStr::from_ptr(expr_ptr).to_string_lossy().to_string();
            if !expr_str.is_empty() {
                self.set_expression(&expr_str);
            }
        }

        // Enabled
        self.enabled = obs_data_get_bool(
            settings,
            b"enabled\0".as_ptr() as *const c_char,
        );

        // Seed
        self.seed = obs_data_get_int(
            settings,
            b"seed\0".as_ptr() as *const c_char,
        ) as u64;
    }

    /// Parse and store an expression.
    fn set_expression(&mut self, expr: &str) {
        match parser::shunting_yard(expr) {
            Ok(tokens) if !tokens.is_empty() => {
                let count = tokens.len();
                self.expressions = vec![(expr.to_string(), tokens)];
                unsafe {
                    blog(
                        LOG_INFO as i32,
                        b"[glitch] expression set (%d tokens)\0".as_ptr() as *const c_char,
                        count as u32,
                    );
                }
            }
            Ok(_) => unsafe {
                blog(
                    LOG_WARNING as i32,
                    b"[glitch] expression parsed to empty tokens, keeping previous\0".as_ptr()
                        as *const c_char,
                );
            },
            Err(_) => unsafe {
                blog(
                    LOG_WARNING as i32,
                    b"[glitch] expression parse failed, keeping previous\0".as_ptr()
                        as *const c_char,
                );
            },
        }
    }

    /// Drain pending commands from the WebSocket command queue.
    /// Name of the OBS source this filter is attached to (for routing).
    unsafe fn parent_source_name(&self) -> Option<String> {
        let parent = obs_filter_get_parent(self.source);
        if parent.is_null() {
            return None;
        }
        let name_ptr = obs_source_get_name(parent);
        if name_ptr.is_null() {
            return None;
        }
        Some(CStr::from_ptr(name_ptr).to_string_lossy().into_owned())
    }

    fn drain_commands(&mut self) {
        let my_source = unsafe { self.parent_source_name() };

        let all_cmds = {
            let mut q = match websocket::CMD_QUEUE.lock() {
                Ok(q) => q,
                Err(_) => return,
            };
            if q.is_empty() {
                return;
            }
            std::mem::take(&mut *q)
        };

        let mut remaining = Vec::new();

        for cmd in all_cmds {
            let addressed_to_me = match cmd.target() {
                None => true,
                Some(t) => my_source.as_deref() == Some(t),
            };

            if !addressed_to_me {
                remaining.push(cmd);
                continue;
            }

            match cmd {
                Command::SetExpression { expr, .. } => {
                    unsafe {
                        blog(
                            LOG_INFO as i32,
                            b"[glitch] ws cmd: set_expression\0".as_ptr() as *const c_char,
                        );
                    }
                    self.saved_expressions = None;
                    self.revert_at = None;
                    self.set_expression(&expr);
                }
                Command::SetEnabled { enabled, .. } => {
                    unsafe {
                        blog(
                            LOG_INFO as i32,
                            b"[glitch] ws cmd: set_enabled\0".as_ptr() as *const c_char,
                        );
                    }
                    self.enabled = enabled;
                }
                Command::Pulse {
                    expr, duration_ms, ..
                } => {
                    unsafe {
                        blog(
                            LOG_INFO as i32,
                            b"[glitch] ws cmd: pulse\0".as_ptr() as *const c_char,
                        );
                    }
                    if self.saved_expressions.is_none() {
                        self.saved_expressions = Some(self.expressions.clone());
                    }
                    self.revert_at =
                        Some(Instant::now() + std::time::Duration::from_millis(duration_ms));
                    self.set_expression(&expr);
                }
            }
        }

        // Put back commands addressed to other sources.
        if !remaining.is_empty() {
            if let Ok(mut q) = websocket::CMD_QUEUE.lock() {
                remaining.append(&mut *q);
                *q = remaining;
            }
        }
    }

    /// Check if a pulse timer has expired and revert to the saved expression.
    fn check_revert(&mut self) {
        if let Some(deadline) = self.revert_at {
            if Instant::now() >= deadline {
                if let Some(saved) = self.saved_expressions.take() {
                    self.expressions = saved;
                    unsafe {
                        blog(
                            LOG_INFO as i32,
                            b"[glitch] pulse expired, reverted to saved expression\0".as_ptr()
                                as *const c_char,
                        );
                    }
                }
                self.revert_at = None;
            }
        }
    }

    /// Push the current filter state to the global snapshot map (keyed by
    /// parent source name) so `get_state` can read it without locking.
    fn update_snapshot(&self) {
        let key = unsafe { self.parent_source_name() }
            .unwrap_or_else(|| "unknown".to_string());
        let expr = self
            .expressions
            .first()
            .map(|(s, _)| s.clone())
            .unwrap_or_default();
        if let Ok(mut snap) = websocket::FILTER_SNAPSHOT.lock() {
            snap.insert(
                key,
                FilterSnapshot {
                    expression: expr,
                    enabled: self.enabled,
                    seed: self.seed,
                    frame_count: self.frame_count,
                },
            );
        }
    }

    /// Remove this filter's snapshot entry when the filter is destroyed.
    pub fn remove_snapshot(&self) {
        let key = unsafe { self.parent_source_name() };
        if let Some(key) = key {
            if let Ok(mut snap) = websocket::FILTER_SNAPSHOT.lock() {
                snap.remove(&key);
            }
        }
    }

    pub unsafe fn destroy_resources(self) {
        obs_enter_graphics();
        if !self.texrender.is_null() {
            gs_texrender_destroy(self.texrender);
        }
        if !self.stagesurface.is_null() {
            gs_stagesurface_destroy(self.stagesurface);
        }
        if !self.output_texture.is_null() {
            gs_texture_destroy(self.output_texture);
        }
        obs_leave_graphics();
    }

    // ─── Video Render (GPU staging + glitch for sync sources) ────

    /// For sync sources (display/window/game capture): stages the GPU
    /// texture to CPU, runs the glitch engine, uploads back, and draws.
    /// For async sources (webcam/media): passthrough — filter_video
    /// already processed the frame.
    pub unsafe fn render(&mut self) {
        websocket::ensure_registered();
        self.drain_commands();
        self.check_revert();
        self.update_snapshot();

        let target = obs_filter_get_target(self.source);
        if target.is_null() {
            obs_source_skip_video_filter(self.source);
            return;
        }

        let cx = obs_source_get_base_width(target);
        let cy = obs_source_get_base_height(target);

        if cx == 0 || cy == 0 {
            obs_source_skip_video_filter(self.source);
            return;
        }

        if !self.enabled || self.expressions.is_empty() || self.async_source {
            obs_source_skip_video_filter(self.source);
            return;
        }

        // ── Frame skipping: only run the full pipeline every N frames,
        //    redraw the cached output texture on intermediate frames. ──

        let size_changed =
            self.render_width != cx || self.render_height != cy;
        let should_process = self.output_texture.is_null()
            || size_changed
            || self.frame_count % RENDER_PROCESS_INTERVAL == 0;

        if !should_process {
            self.draw_output(cx, cy);
            self.frame_count += 1;
            return;
        }

        // ── Step 1: Render parent source into our texrender ──

        if self.texrender.is_null() {
            self.texrender = gs_texrender_create(
                gs_color_format_GS_RGBA,
                gs_zstencil_format_GS_ZS_NONE,
            );
        }
        if self.texrender.is_null() {
            obs_source_skip_video_filter(self.source);
            return;
        }

        gs_texrender_reset(self.texrender);
        if !gs_texrender_begin(self.texrender, cx, cy) {
            obs_source_skip_video_filter(self.source);
            return;
        }

        gs_ortho(0.0, cx as f32, 0.0, cy as f32, -100.0, 100.0);
        obs_source_default_render(target);
        gs_texrender_end(self.texrender);

        let tex = gs_texrender_get_texture(self.texrender);
        if tex.is_null() {
            obs_source_skip_video_filter(self.source);
            return;
        }

        // ── Step 2: Stage GPU texture → CPU buffer ──

        if self.render_width != cx || self.render_height != cy {
            if !self.stagesurface.is_null() {
                gs_stagesurface_destroy(self.stagesurface);
                self.stagesurface = ptr::null_mut();
            }
            if !self.output_texture.is_null() {
                gs_texture_destroy(self.output_texture);
                self.output_texture = ptr::null_mut();
            }
            self.render_width = cx;
            self.render_height = cy;
        }

        if self.stagesurface.is_null() {
            self.stagesurface =
                gs_stagesurface_create(cx, cy, gs_color_format_GS_RGBA);
        }
        if self.stagesurface.is_null() {
            obs_source_skip_video_filter(self.source);
            return;
        }

        gs_stage_texture(self.stagesurface, tex);

        let mut data: *mut u8 = ptr::null_mut();
        let mut linesize: u32 = 0;
        if !gs_stagesurface_map(self.stagesurface, &mut data, &mut linesize) {
            obs_source_skip_video_filter(self.source);
            return;
        }

        let stride = cx as usize * 4;
        self.buffer.resize(stride * cy as usize, 0);
        for row in 0..cy as usize {
            let src_off = row * linesize as usize;
            let dst_off = row * stride;
            let src = std::slice::from_raw_parts(data.add(src_off), stride);
            self.buffer[dst_off..dst_off + stride].copy_from_slice(src);
        }

        gs_stagesurface_unmap(self.stagesurface);

        // ── Step 3: Process pixels with glitch-core ──

        self.process_frame(cx, cy);

        // ── Step 4: Upload processed pixels to output texture ──

        if self.output_texture.is_null() {
            self.output_texture = gs_texture_create(
                cx,
                cy,
                gs_color_format_GS_RGBA,
                1,
                ptr::null_mut(),
                GS_DYNAMIC,
            );
        }
        if self.output_texture.is_null() {
            obs_source_skip_video_filter(self.source);
            return;
        }

        gs_texture_set_image(
            self.output_texture,
            self.buffer.as_ptr(),
            stride as u32,
            false,
        );

        // ── Step 5: Draw processed texture ──

        self.draw_output(cx, cy);
        self.frame_count += 1;
    }

    /// Draw the cached output texture with the default effect.
    unsafe fn draw_output(&self, cx: u32, cy: u32) {
        if self.output_texture.is_null() {
            obs_source_skip_video_filter(self.source);
            return;
        }
        let effect =
            obs_get_base_effect(obs_base_effect_OBS_EFFECT_DEFAULT);
        if effect.is_null() {
            obs_source_skip_video_filter(self.source);
            return;
        }
        let image_param = gs_effect_get_param_by_name(
            effect,
            b"image\0".as_ptr() as *const c_char,
        );
        while gs_effect_loop(effect, b"Draw\0".as_ptr() as *const c_char)
        {
            gs_effect_set_texture(image_param, self.output_texture);
            gs_draw_sprite(self.output_texture, 0, cx, cy);
        }
    }

    // ─── Filter Video (async CPU frame processing) ──────────────

    /// Process an async video frame directly on the CPU.
    /// Called by OBS for each frame from async sources (PipeWire, webcam, etc.).
    pub unsafe fn filter_frame(&mut self, frame: *mut obs_source_frame) {
        self.drain_commands();
        self.check_revert();
        self.update_snapshot();

        self.async_source = true;
        let frame = &mut *frame;

        if !self.enabled || self.expressions.is_empty() {
            return;
        }

        let width = frame.width;
        let height = frame.height;
        let data = frame.data[0];
        let linesize = frame.linesize[0];
        let format = frame.format;

        if data.is_null() || width == 0 || height == 0 {
            return;
        }

        // Only handle packed RGBA/BGRA formats (single plane, 4 bytes/pixel)
        let is_bgra = format == video_format_VIDEO_FORMAT_BGRA;
        let is_rgba = format == video_format_VIDEO_FORMAT_RGBA;

        if !is_bgra && !is_rgba {
            // Skip unsupported formats (NV12, I420, etc.)
            if self.frame_count < 3 {
                blog(
                    LOG_WARNING as i32,
                    b"[glitch] unsupported format: %d, skipping\0".as_ptr() as *const c_char,
                    format as u32,
                );
            }
            return;
        }

        let stride = width as usize * 4;
        self.buffer.resize(stride * height as usize, 0);

        // ── Copy frame to buffer (converting BGRA→RGBA if needed) ──
        for row in 0..height as usize {
            let src_off = row * linesize as usize;
            let dst_off = row * stride;
            let src = std::slice::from_raw_parts(data.add(src_off), stride);
            self.buffer[dst_off..dst_off + stride].copy_from_slice(src);

            if is_bgra {
                // Swap B↔R for each pixel in this row
                for px in 0..width as usize {
                    let i = dst_off + px * 4;
                    self.buffer.swap(i, i + 2);
                }
            }
        }

        if self.frame_count < 2 {
            let mid = (height as usize / 2) * stride + (width as usize / 2) * 4;
            blog(
                LOG_INFO as i32,
                b"[glitch] filter_video frame %d: %dx%d fmt=%d center RGBA: %d %d %d %d\0".as_ptr() as *const c_char,
                self.frame_count as u32,
                width, height, format as u32,
                self.buffer[mid] as u32,
                self.buffer[mid + 1] as u32,
                self.buffer[mid + 2] as u32,
                self.buffer[mid + 3] as u32,
            );
        }

        // ── Process with glitch-core ──
        self.process_frame(width, height);

        // ── Write processed pixels back to frame (RGBA→BGRA if needed) ──
        for row in 0..height as usize {
            let buf_off = row * stride;
            let dst_off = row * linesize as usize;

            if is_bgra {
                // Swap R↔B back before writing
                for px in 0..width as usize {
                    let i = buf_off + px * 4;
                    self.buffer.swap(i, i + 2);
                }
            }

            let dst = std::slice::from_raw_parts_mut(data.add(dst_off), stride);
            dst.copy_from_slice(&self.buffer[buf_off..buf_off + stride]);
        }

        self.frame_count += 1;
    }

    // ─── Frame Processing ─────────────────────────────────────────

    /// Process the pixel buffer using glitch-core expressions.
    ///
    /// Hot-path optimisations:
    /// - eval_fast on raw &[u8] — no DynamicImage enum dispatch
    /// - rayon par_chunks_mut — rows processed across all cores
    /// - per-thread stack + RNG — zero contention, zero sync
    /// - ping-pong between two persistent buffers (no image wrapping)
    fn process_frame(&mut self, width: u32, height: u32) {
        if self.expressions.is_empty() {
            return;
        }

        let stride = width as usize * 4;
        let buf_size = stride * height as usize;
        self.process_buf.resize(buf_size, 0);

        let base_seed = self.seed.wrapping_add(self.frame_count);

        let mut buf_a = std::mem::take(&mut self.buffer);
        let mut buf_b = std::mem::take(&mut self.process_buf);

        for (pass, (_, tokens)) in self.expressions.iter().enumerate() {
            let pass_seed = base_seed.wrapping_add(pass as u64 * 0x9E37_79B9_7F4A_7C15);
            if pass % 2 == 0 {
                Self::eval_pass(tokens, width, height, stride, &buf_a, &mut buf_b, pass_seed);
            } else {
                Self::eval_pass(tokens, width, height, stride, &buf_b, &mut buf_a, pass_seed);
            }
        }

        let num = self.expressions.len();
        if num % 2 == 1 {
            self.buffer = buf_b;
            self.process_buf = buf_a;
        } else {
            self.buffer = buf_a;
            self.process_buf = buf_b;
        }
    }

    /// Parallel per-row evaluation. Each row gets its own deterministic RNG
    /// (seeded from base_seed + row index) and its own eval stack — no shared
    /// mutable state means zero synchronisation overhead.
    fn eval_pass(
        tokens: &[Token],
        width: u32,
        height: u32,
        stride: usize,
        src: &[u8],
        dst: &mut [u8],
        base_seed: u64,
    ) {
        dst.par_chunks_mut(stride)
            .enumerate()
            .for_each(|(y, row)| {
                let mut rng = StdRng::seed_from_u64(base_seed.wrapping_add(y as u64));
                let mut stack: Vec<Rgb> = Vec::with_capacity(32);
                let y = y as u32;
                let row_off = y as usize * stride;

                for x in 0..width {
                    let local = x as usize * 4;
                    let src_idx = row_off + local;
                    let r = src[src_idx];
                    let g = src[src_idx + 1];
                    let b = src[src_idx + 2];
                    let a = src[src_idx + 3];

                    if a == 0 {
                        row[local..local + 4].copy_from_slice(&[0, 0, 0, 0]);
                        continue;
                    }

                    match eval::eval_fast(
                        EvalContext {
                            tokens,
                            size: (width, height),
                            rgba: Rgba([r, g, b, a]),
                            saved_rgb: [0, 0, 0],
                            position: (x, y),
                            ignore_state: true,
                        },
                        src,
                        width,
                        height,
                        &mut stack,
                        &mut rng,
                    ) {
                        Ok(c) => {
                            row[local] = c[0];
                            row[local + 1] = c[1];
                            row[local + 2] = c[2];
                            row[local + 3] = c[3];
                        }
                        Err(_) => {
                            row[local..local + 4].copy_from_slice(&[r, g, b, a]);
                        }
                    }
                }
            });
    }
}
