//! OBS Glitch Filter Plugin — raw obs-sys FFI
//!
//! Registers a video filter source that applies glitch-core expressions
//! to live video frames. Uses the same eval pipeline as glitch-wasm.

use std::ffi::{c_char, c_void};
use std::mem;
use std::ptr;

use obs_sys::*;

mod engine;
mod websocket;
use engine::GlitchFilter;

// ─── Constants ───────────────────────────────────────────────────

const FILTER_ID: *const c_char = b"glitch_filter\0".as_ptr() as *const c_char;
const FILTER_NAME: *const c_char = b"Glitch\0".as_ptr() as *const c_char;
const DEFAULT_EXPRESSION: *const c_char = b"(c & (c ^ 55)) + 25\0".as_ptr() as *const c_char;

const SETTING_EXPRESSION: *const c_char = b"expression\0".as_ptr() as *const c_char;
const SETTING_ENABLED: *const c_char = b"enabled\0".as_ptr() as *const c_char;
const SETTING_SEED: *const c_char = b"seed\0".as_ptr() as *const c_char;

// ─── OBS Module Exports ─────────────────────────────────────────

/// Module pointer storage (required by OBS module API).
static mut OBS_MODULE_POINTER: *mut obs_module_t = ptr::null_mut();

#[no_mangle]
pub unsafe extern "C" fn obs_module_set_pointer(module: *mut obs_module_t) {
    OBS_MODULE_POINTER = module;
}

#[no_mangle]
pub unsafe extern "C" fn obs_module_ver() -> u32 {
    // OBS module API version — LIBOBS_API_MAJOR_VER << 24 | ...
    // Use 0 to indicate "compatible with any"
    (29 << 24) | (1 << 16)
}

#[no_mangle]
pub unsafe extern "C" fn obs_module_name() -> *const c_char {
    b"glitch\0".as_ptr() as *const c_char
}

#[no_mangle]
pub unsafe extern "C" fn obs_module_description() -> *const c_char {
    b"Real-time pixel glitching via expression engine\0".as_ptr() as *const c_char
}

/// Called after all modules have loaded — registers the WebSocket vendor.
/// Must happen here (not in obs_module_load) because obs-websocket sets up
/// its proc handlers during its own obs_module_load.
#[no_mangle]
pub unsafe extern "C" fn obs_module_post_load() {
    blog(
        LOG_INFO as i32,
        b"[glitch] post_load: registering websocket vendor\0".as_ptr() as *const c_char,
    );
    websocket::register_vendor();
}

/// Plugin entry point — registers the glitch filter source.
#[no_mangle]
pub unsafe extern "C" fn obs_module_load() -> bool {
    blog(
        LOG_INFO as i32,
        b"[glitch] module loading (v%s)\0".as_ptr() as *const c_char,
        b"0.1.0\0".as_ptr() as *const c_char,
    );

    let mut info: obs_source_info = mem::zeroed();

    info.id = FILTER_ID;
    info.type_ = obs_source_type_OBS_SOURCE_TYPE_FILTER;
    info.output_flags = OBS_SOURCE_VIDEO;

    info.get_name = Some(glitch_get_name);
    info.create = Some(glitch_create);
    info.destroy = Some(glitch_destroy);
    info.video_render = Some(glitch_video_render);
    info.filter_video = Some(glitch_filter_video);
    info.get_properties = Some(glitch_get_properties);
    info.get_defaults = Some(glitch_get_defaults);
    info.update = Some(glitch_update);

    obs_register_source_s(&info, mem::size_of::<obs_source_info>() as u64);

    blog(
        LOG_INFO as i32,
        b"[glitch] filter source registered\0".as_ptr() as *const c_char,
    );
    true
}

// ─── Source Callbacks ────────────────────────────────────────────

/// Returns the display name for the filter ("Glitch").
unsafe extern "C" fn glitch_get_name(_type_data: *mut c_void) -> *const c_char {
    FILTER_NAME
}

/// Creates a new filter instance. Allocates state on the heap
/// and returns an opaque pointer that OBS will pass to all callbacks.
unsafe extern "C" fn glitch_create(
    settings: *mut obs_data_t,
    source: *mut obs_source_t,
) -> *mut c_void {
    blog(
        LOG_INFO as i32,
        b"[glitch] filter instance created\0".as_ptr() as *const c_char,
    );
    let filter = GlitchFilter::new(source, settings);
    Box::into_raw(Box::new(filter)) as *mut c_void
}

/// Destroys the filter instance. Cleans up GPU resources.
unsafe extern "C" fn glitch_destroy(data: *mut c_void) {
    if data.is_null() {
        return;
    }
    blog(
        LOG_INFO as i32,
        b"[glitch] filter instance destroyed\0".as_ptr() as *const c_char,
    );
    let filter = Box::from_raw(data as *mut GlitchFilter);
    filter.remove_snapshot();
    filter.destroy_resources();
}

/// Called every frame — renders the composited output.
/// The actual pixel processing happens in filter_video for async sources.
unsafe extern "C" fn glitch_video_render(data: *mut c_void, _effect: *mut gs_effect_t) {
    if data.is_null() {
        return;
    }
    let filter = &mut *(data as *mut GlitchFilter);
    filter.render();
}

/// Called for each async video frame (PipeWire, webcam, etc.).
/// Receives raw CPU pixel data — no GPU staging needed.
unsafe extern "C" fn glitch_filter_video(
    data: *mut c_void,
    frame: *mut obs_source_frame,
) -> *mut obs_source_frame {
    if data.is_null() || frame.is_null() {
        return frame;
    }
    let filter = &mut *(data as *mut GlitchFilter);
    filter.filter_frame(frame);
    frame
}

/// Builds the OBS properties panel (expression text box, enable toggle, seed).
unsafe extern "C" fn glitch_get_properties(_data: *mut c_void) -> *mut obs_properties_t {
    let props = obs_properties_create();

    obs_properties_add_text(
        props,
        SETTING_EXPRESSION,
        b"Expression\0".as_ptr() as *const c_char,
        obs_text_type_OBS_TEXT_DEFAULT,
    );

    obs_properties_add_bool(
        props,
        SETTING_ENABLED,
        b"Enable Effect\0".as_ptr() as *const c_char,
    );

    obs_properties_add_int(
        props,
        SETTING_SEED,
        b"RNG Seed\0".as_ptr() as *const c_char,
        0,
        10000,
        1,
    );

    props
}

/// Sets default values for filter settings.
unsafe extern "C" fn glitch_get_defaults(settings: *mut obs_data_t) {
    obs_data_set_default_string(settings, SETTING_EXPRESSION, DEFAULT_EXPRESSION);
    obs_data_set_default_bool(settings, SETTING_ENABLED, true);
    obs_data_set_default_int(settings, SETTING_SEED, 42);
}

/// Called when the user changes settings in the properties panel.
unsafe extern "C" fn glitch_update(data: *mut c_void, settings: *mut obs_data_t) {
    if data.is_null() {
        return;
    }
    let filter = &mut *(data as *mut GlitchFilter);
    filter.update_settings(settings);
}
