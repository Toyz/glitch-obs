//! OBS WebSocket vendor API integration.
//!
//! Registers "glitch" as a vendor so external clients (Twitch bots,
//! Stream Deck, etc.) can control the filter via `CallVendorRequest`.
//!
//! Commands are delivered through a global queue that the render thread
//! drains each frame — no lock contention on the hot path.
//!
//! When multiple filter instances exist, each WebSocket request may carry
//! an optional `"source"` field (the OBS parent source name) to target a
//! specific filter.  If omitted, the command is broadcast to all filters.

use obs_sys::*;
use std::collections::HashMap;
use std::ffi::{c_char, c_void, CStr, CString};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};

// ─── C shim (compiled from ws_shim.c) ─────────────────────────────
//
// The obs-websocket-api.h functions are all `static inline`, invisible
// to bindgen.  ws_shim.c wraps them as real symbols we can link to.

extern "C" {
    fn ws_shim_register_vendor(name: *const c_char) -> *mut c_void;
    fn ws_shim_register_request(
        vendor: *mut c_void,
        request_type: *const c_char,
        callback: unsafe extern "C" fn(*mut obs_data_t, *mut obs_data_t, *mut c_void),
        priv_data: *mut c_void,
    ) -> bool;
}

// ─── Command Queue ───────────────────────────────────────────────

pub enum Command {
    SetExpression {
        target: Option<String>,
        expr: String,
    },
    SetEnabled {
        target: Option<String>,
        enabled: bool,
    },
    Pulse {
        target: Option<String>,
        expr: String,
        duration_ms: u64,
    },
}

impl Command {
    /// The OBS parent source name this command is addressed to, or
    /// `None` for broadcast (all filter instances).
    pub fn target(&self) -> Option<&str> {
        match self {
            Command::SetExpression { target, .. }
            | Command::SetEnabled { target, .. }
            | Command::Pulse { target, .. } => target.as_deref(),
        }
    }
}

pub static CMD_QUEUE: Mutex<Vec<Command>> = Mutex::new(Vec::new());

static VENDOR_REGISTERED: AtomicBool = AtomicBool::new(false);

/// Call from the render/filter path to retry registration if it
/// failed at post_load (e.g. obs-websocket loaded late).
pub unsafe fn ensure_registered() {
    if VENDOR_REGISTERED.load(Ordering::Relaxed) {
        return;
    }
    register_vendor();
}

// ─── Filter State Snapshots (one per source) ─────────────────────

pub struct FilterSnapshot {
    pub expression: String,
    pub enabled: bool,
    pub seed: u64,
    pub frame_count: u64,
}

/// Keyed by the OBS parent source name (e.g. "Camera", "Screen Capture").
pub static FILTER_SNAPSHOT: LazyLock<Mutex<HashMap<String, FilterSnapshot>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

// ─── Vendor Registration ─────────────────────────────────────────

pub unsafe fn register_vendor() {
    let vendor = ws_shim_register_vendor(b"glitch\0".as_ptr() as *const c_char);
    if vendor.is_null() {
        blog(
            LOG_WARNING as i32,
            b"[glitch] vendor registration failed (obs-websocket not available?)\0".as_ptr()
                as *const c_char,
        );
        return;
    }

    let r1 = ws_shim_register_request(
        vendor,
        b"set_expression\0".as_ptr() as *const c_char,
        handle_set_expression,
        std::ptr::null_mut(),
    );
    let r2 = ws_shim_register_request(
        vendor,
        b"set_enabled\0".as_ptr() as *const c_char,
        handle_set_enabled,
        std::ptr::null_mut(),
    );
    let r3 = ws_shim_register_request(
        vendor,
        b"pulse\0".as_ptr() as *const c_char,
        handle_pulse,
        std::ptr::null_mut(),
    );
    let r4 = ws_shim_register_request(
        vendor,
        b"get_state\0".as_ptr() as *const c_char,
        handle_get_state,
        std::ptr::null_mut(),
    );

    VENDOR_REGISTERED.store(true, Ordering::Relaxed);

    if r1 && r2 && r3 && r4 {
        blog(
            LOG_INFO as i32,
            b"[glitch] websocket vendor 'glitch' registered (4 requests)\0".as_ptr()
                as *const c_char,
        );
    } else {
        blog(
            LOG_WARNING as i32,
            b"[glitch] vendor registered but some requests failed\0".as_ptr()
                as *const c_char,
        );
    }
}

// ─── Helpers ─────────────────────────────────────────────────────

/// Read the optional `"source"` key from a request.  Returns `None`
/// when the key is absent or empty (meaning "broadcast to all").
unsafe fn read_target(request: *mut obs_data_t) -> Option<String> {
    let ptr = obs_data_get_string(request, b"source\0".as_ptr() as *const c_char);
    if ptr.is_null() {
        return None;
    }
    let s = CStr::from_ptr(ptr).to_string_lossy();
    if s.is_empty() {
        None
    } else {
        Some(s.into_owned())
    }
}

// ─── Request Handlers ────────────────────────────────────────────

unsafe extern "C" fn handle_set_expression(
    request: *mut obs_data_t,
    response: *mut obs_data_t,
    _priv: *mut c_void,
) {
    let expr_ptr = obs_data_get_string(request, b"expression\0".as_ptr() as *const c_char);
    if expr_ptr.is_null() {
        obs_data_set_bool(response, b"ok\0".as_ptr() as *const c_char, false);
        obs_data_set_string(
            response,
            b"error\0".as_ptr() as *const c_char,
            b"missing 'expression'\0".as_ptr() as *const c_char,
        );
        return;
    }

    let target = read_target(request);
    let expr = CStr::from_ptr(expr_ptr).to_string_lossy().to_string();
    blog(
        LOG_INFO as i32,
        b"[glitch] ws request: set_expression\0".as_ptr() as *const c_char,
    );
    if let Ok(mut q) = CMD_QUEUE.lock() {
        q.push(Command::SetExpression { target, expr });
    }
    obs_data_set_bool(response, b"ok\0".as_ptr() as *const c_char, true);
}

unsafe extern "C" fn handle_set_enabled(
    request: *mut obs_data_t,
    response: *mut obs_data_t,
    _priv: *mut c_void,
) {
    let enabled = obs_data_get_bool(request, b"enabled\0".as_ptr() as *const c_char);
    let target = read_target(request);
    blog(
        LOG_INFO as i32,
        b"[glitch] ws request: set_enabled\0".as_ptr() as *const c_char,
    );
    if let Ok(mut q) = CMD_QUEUE.lock() {
        q.push(Command::SetEnabled { target, enabled });
    }
    obs_data_set_bool(response, b"ok\0".as_ptr() as *const c_char, true);
}

unsafe extern "C" fn handle_pulse(
    request: *mut obs_data_t,
    response: *mut obs_data_t,
    _priv: *mut c_void,
) {
    let expr_ptr = obs_data_get_string(request, b"expression\0".as_ptr() as *const c_char);
    if expr_ptr.is_null() {
        obs_data_set_bool(response, b"ok\0".as_ptr() as *const c_char, false);
        obs_data_set_string(
            response,
            b"error\0".as_ptr() as *const c_char,
            b"missing 'expression'\0".as_ptr() as *const c_char,
        );
        return;
    }

    let target = read_target(request);
    let expr = CStr::from_ptr(expr_ptr).to_string_lossy().to_string();
    let duration_ms = obs_data_get_int(request, b"duration_ms\0".as_ptr() as *const c_char) as u64;
    let duration_ms = if duration_ms == 0 { 5000 } else { duration_ms };

    blog(
        LOG_INFO as i32,
        b"[glitch] ws request: pulse\0".as_ptr() as *const c_char,
    );
    if let Ok(mut q) = CMD_QUEUE.lock() {
        q.push(Command::Pulse {
            target,
            expr,
            duration_ms,
        });
    }
    obs_data_set_bool(response, b"ok\0".as_ptr() as *const c_char, true);
}

unsafe extern "C" fn handle_get_state(
    request: *mut obs_data_t,
    response: *mut obs_data_t,
    _priv: *mut c_void,
) {
    blog(
        LOG_INFO as i32,
        b"[glitch] ws request: get_state\0".as_ptr() as *const c_char,
    );

    let target = read_target(request);

    let snap = match FILTER_SNAPSHOT.lock() {
        Ok(s) => s,
        Err(_) => {
            obs_data_set_bool(response, b"ok\0".as_ptr() as *const c_char, false);
            return;
        }
    };

    // Always include the list of active source names so the caller
    // knows what targets are available.
    let sources_csv: String = snap.keys().cloned().collect::<Vec<_>>().join(",");
    let sources_c = CString::new(sources_csv).unwrap_or_default();
    obs_data_set_string(
        response,
        b"sources\0".as_ptr() as *const c_char,
        sources_c.as_ptr(),
    );

    // Pick the requested filter (by source name) or fall back to first.
    let entry: Option<(&String, &FilterSnapshot)> = if let Some(ref t) = target {
        snap.get_key_value(t)
    } else {
        snap.iter().next()
    };

    match entry {
        Some((src, s)) => {
            let src_c = CString::new(src.as_str()).unwrap_or_default();
            obs_data_set_string(
                response,
                b"source\0".as_ptr() as *const c_char,
                src_c.as_ptr(),
            );
            let expr_c = CString::new(s.expression.as_str()).unwrap_or_default();
            obs_data_set_string(
                response,
                b"expression\0".as_ptr() as *const c_char,
                expr_c.as_ptr(),
            );
            obs_data_set_bool(
                response,
                b"enabled\0".as_ptr() as *const c_char,
                s.enabled,
            );
            obs_data_set_int(
                response,
                b"seed\0".as_ptr() as *const c_char,
                s.seed as i64,
            );
            obs_data_set_int(
                response,
                b"frame_count\0".as_ptr() as *const c_char,
                s.frame_count as i64,
            );
            obs_data_set_bool(response, b"ok\0".as_ptr() as *const c_char, true);
        }
        None => {
            obs_data_set_bool(response, b"ok\0".as_ptr() as *const c_char, false);
            obs_data_set_string(
                response,
                b"error\0".as_ptr() as *const c_char,
                b"no active filter\0".as_ptr() as *const c_char,
            );
        }
    }
}
