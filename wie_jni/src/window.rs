#![allow(non_snake_case)]
//! Presents emulator frames on the Android `Surface` handed over by `GameView`.
//!
//! Only a handful of `libandroid` functions are needed, so they are declared here
//! directly. The `#[link]` attribute is applied on Android only, which keeps the
//! crate buildable (and testable) on a desktop host.

use std::{ffi::c_void, sync::Mutex};

use jni::{JNIEnv, objects::JObject};

#[repr(C)]
pub(crate) struct ANativeWindowBuffer {
    width: i32,
    height: i32,
    stride: i32,
    format: i32,
    bits: *mut c_void,
    reserved: [u32; 6],
}

const WINDOW_FORMAT_RGBA_8888: i32 = 1;

#[cfg(target_os = "android")]
#[link(name = "android")]
unsafe extern "C" {
    fn ANativeWindow_fromSurface(env: *mut jni::sys::JNIEnv, surface: jni::sys::jobject) -> *mut c_void;
    fn ANativeWindow_release(window: *mut c_void);
    fn ANativeWindow_setBuffersGeometry(window: *mut c_void, width: i32, height: i32, format: i32) -> i32;
    fn ANativeWindow_lock(window: *mut c_void, out_buffer: *mut ANativeWindowBuffer, in_out_dirty_bounds: *mut c_void) -> i32;
    fn ANativeWindow_unlockAndPost(window: *mut c_void) -> i32;
}

// Host builds (tests, headless runs) have no surface; frames are only kept in memory.
#[cfg(not(target_os = "android"))]
mod host {
    use std::ffi::c_void;

    pub unsafe fn ANativeWindow_fromSurface(_env: *mut jni::sys::JNIEnv, _surface: jni::sys::jobject) -> *mut c_void {
        std::ptr::null_mut()
    }
    pub unsafe fn ANativeWindow_release(_window: *mut c_void) {}
    pub unsafe fn ANativeWindow_setBuffersGeometry(_window: *mut c_void, _width: i32, _height: i32, _format: i32) -> i32 {
        -1
    }
    pub unsafe fn ANativeWindow_lock(_window: *mut c_void, _out_buffer: *mut super::ANativeWindowBuffer, _dirty: *mut c_void) -> i32 {
        -1
    }
    pub unsafe fn ANativeWindow_unlockAndPost(_window: *mut c_void) -> i32 {
        -1
    }
}
#[cfg(not(target_os = "android"))]
use host::*;

/// The last emulator frame as RGBA bytes, kept so it can be redrawn on a new surface.
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

struct State {
    window: usize, // ANativeWindow*, 0 when absent
    geometry: (u32, u32),
    frame: Option<Frame>,
}

static STATE: Mutex<State> = Mutex::new(State {
    window: 0,
    geometry: (0, 0),
    frame: None,
});

fn state() -> std::sync::MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// Called from the UI thread whenever the surface is created, changed or destroyed (null).
pub fn set_surface(env: &JNIEnv<'_>, surface: &JObject<'_>) {
    let mut state = state();

    if state.window != 0 {
        unsafe { ANativeWindow_release(state.window as *mut c_void) };
        state.window = 0;
        state.geometry = (0, 0);
    }

    if surface.is_null() {
        return;
    }

    let window = unsafe { ANativeWindow_fromSurface(env.get_raw(), surface.as_raw()) };
    if window.is_null() {
        tracing::error!("[window] ANativeWindow_fromSurface returned null");
        return;
    }
    state.window = window as usize;
    present_locked(&mut state);
}

/// Stores a new frame and draws it if a surface is available.
pub fn present(frame: Frame) {
    let mut state = state();
    state.frame = Some(frame);
    present_locked(&mut state);
}

/// Copy of the most recent frame, used by host-side tests.
pub fn last_frame() -> Option<Frame> {
    state().frame.as_ref().map(|frame| Frame {
        width: frame.width,
        height: frame.height,
        rgba: frame.rgba.clone(),
    })
}

fn present_locked(state: &mut State) {
    let window = state.window as *mut c_void;
    if window.is_null() {
        return;
    }
    let Some(frame) = state.frame.as_ref() else { return };
    let (width, height) = (frame.width, frame.height);

    if state.geometry != (width, height) {
        // The compositor scales this buffer to the GameView's size.
        let result = unsafe { ANativeWindow_setBuffersGeometry(window, width as i32, height as i32, WINDOW_FORMAT_RGBA_8888) };
        if result != 0 {
            tracing::error!("[window] setBuffersGeometry failed: {result}");
            return;
        }
        state.geometry = (width, height);
    }

    let mut buffer = ANativeWindowBuffer {
        width: 0,
        height: 0,
        stride: 0,
        format: 0,
        bits: std::ptr::null_mut(),
        reserved: [0; 6],
    };
    if unsafe { ANativeWindow_lock(window, &mut buffer, std::ptr::null_mut()) } != 0 || buffer.bits.is_null() {
        tracing::warn!("[window] lock failed");
        return;
    }

    let rows = (buffer.height.max(0) as u32).min(height) as usize;
    let cols = (buffer.width.max(0) as u32).min(width) as usize;
    let stride_bytes = buffer.stride.max(0) as usize * 4;
    let src_row_bytes = width as usize * 4;
    let dst = buffer.bits as *mut u8;
    for y in 0..rows {
        let src = &frame.rgba[y * src_row_bytes..y * src_row_bytes + cols * 4];
        unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), dst.add(y * stride_bytes), cols * 4) };
    }

    unsafe { ANativeWindow_unlockAndPost(window) };
}
