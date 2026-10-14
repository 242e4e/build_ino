//! A diagnostic read-out stamped onto the frame before it reaches the surface.
//!
//! The Java shell's own profile overlay is dead code in the shipped build (nothing
//! calls `startProfileOverlay`, and `profileView` is never created), and logcat needs
//! a root-granted reader. Drawing straight into the frame needs neither, so the engine
//! can report whether the ARM JIT actually took over and how fast the guest is running.
//!
//! Set `WIE_OVERLAY=0` to turn it off.

use std::{
    sync::{
        Mutex,
        atomic::{AtomicU32, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

/// What wie's own logs said about the ARM JIT, as captured by the logging sink.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Jit {
    /// No AOT line seen yet — still preparing, or the backend was never built in.
    Unknown,
    Installed,
    Failed,
}

static JIT: Mutex<Jit> = Mutex::new(Jit::Unknown);
static FRAMES: AtomicU64 = AtomicU64::new(0);
static TICK_MICROS: AtomicU64 = AtomicU64::new(0);
static TICK_COUNT: AtomicU64 = AtomicU64::new(0);
static FPS_TIMES_10: AtomicU32 = AtomicU32::new(0);
static TICK_MS_TIMES_10: AtomicU32 = AtomicU32::new(0);
static WINDOW: Mutex<Option<Instant>> = Mutex::new(None);

fn lock<T>(mutex: &'static Mutex<T>) -> std::sync::MutexGuard<'static, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// Called for every log line so the JIT verdict can be read off wie's own messages.
pub fn note_log_line(line: &str) {
    if !line.contains("AOT") {
        return;
    }
    let verdict = if line.contains("installed") {
        Jit::Installed
    } else if line.contains("failed") {
        Jit::Failed
    } else {
        return;
    };
    *lock(&JIT) = verdict;
}

/// Records one completed `nativeTick`, and recomputes the rates once a second.
pub fn note_tick(elapsed: Duration) {
    TICK_MICROS.fetch_add(elapsed.as_micros() as u64, Ordering::Relaxed);
    TICK_COUNT.fetch_add(1, Ordering::Relaxed);

    let mut window = lock(&WINDOW);
    let start = window.get_or_insert_with(Instant::now);
    let span = start.elapsed();
    if span < Duration::from_secs(1) {
        return;
    }

    let frames = FRAMES.swap(0, Ordering::Relaxed);
    let ticks = TICK_COUNT.swap(0, Ordering::Relaxed);
    let micros = TICK_MICROS.swap(0, Ordering::Relaxed);
    FPS_TIMES_10.store((frames as f64 * 10.0 / span.as_secs_f64()) as u32, Ordering::Relaxed);
    TICK_MS_TIMES_10.store(if ticks == 0 { 0 } else { (micros / ticks / 100) as u32 }, Ordering::Relaxed);
    *window = Some(Instant::now());
}

pub fn note_frame() {
    FRAMES.fetch_add(1, Ordering::Relaxed);
}

fn enabled() -> bool {
    !matches!(std::env::var("WIE_OVERLAY").as_deref(), Ok("0"))
}

/// e.g. `JIT ON 58.2F 4.1MS`
pub fn status_line() -> String {
    let jit = match *lock(&JIT) {
        // No verdict: either the backend was never compiled in, or it is still preparing.
        Jit::Unknown if cfg!(feature = "jit") => "JIT ?",
        Jit::Unknown => "JIT NO",
        Jit::Installed => "JIT ON",
        Jit::Failed => "JIT OFF",
    };
    let fps = FPS_TIMES_10.load(Ordering::Relaxed);
    let tick = TICK_MS_TIMES_10.load(Ordering::Relaxed);
    format!("{jit} {}.{}F {}.{}MS", fps / 10, fps % 10, tick / 10, tick % 10)
}

// A 5x7 bitmap font, one byte per column, low bit at the top. Only the characters the
// status line can produce are defined; anything else is drawn as a blank.
fn glyph(character: char) -> [u8; 5] {
    match character {
        '0' => [0x3e, 0x51, 0x49, 0x45, 0x3e],
        '1' => [0x00, 0x42, 0x7f, 0x40, 0x00],
        '2' => [0x42, 0x61, 0x51, 0x49, 0x46],
        '3' => [0x21, 0x41, 0x45, 0x4b, 0x31],
        '4' => [0x18, 0x14, 0x12, 0x7f, 0x10],
        '5' => [0x27, 0x45, 0x45, 0x45, 0x39],
        '6' => [0x3c, 0x4a, 0x49, 0x49, 0x30],
        '7' => [0x01, 0x71, 0x09, 0x05, 0x03],
        '8' => [0x36, 0x49, 0x49, 0x49, 0x36],
        '9' => [0x06, 0x49, 0x49, 0x29, 0x1e],
        'F' => [0x7f, 0x09, 0x09, 0x09, 0x01],
        'I' => [0x00, 0x41, 0x7f, 0x41, 0x00],
        'J' => [0x20, 0x40, 0x41, 0x3f, 0x01],
        'M' => [0x7f, 0x02, 0x0c, 0x02, 0x7f],
        'N' => [0x7f, 0x04, 0x08, 0x10, 0x7f],
        'O' => [0x3e, 0x41, 0x41, 0x41, 0x3e],
        'S' => [0x46, 0x49, 0x49, 0x49, 0x31],
        'T' => [0x01, 0x01, 0x7f, 0x01, 0x01],
        '.' => [0x00, 0x60, 0x60, 0x00, 0x00],
        '?' => [0x02, 0x01, 0x51, 0x09, 0x06],
        _ => [0; 5],
    }
}

/// Stamps `text` into an RGBA buffer at the top-left, over a black band.
pub fn draw(rgba: &mut [u8], width: u32, height: u32, text: &str) {
    if !enabled() || width < 8 || height < 10 {
        return;
    }
    let (width, height) = (width as usize, height as usize);
    let put = |rgba: &mut [u8], x: usize, y: usize, value: [u8; 3]| {
        if x >= width || y >= height {
            return;
        }
        let offset = (y * width + x) * 4;
        if let Some(pixel) = rgba.get_mut(offset..offset + 3) {
            pixel.copy_from_slice(&value);
        }
    };

    for y in 0..9 {
        for x in 0..width {
            put(rgba, x, y, [0, 0, 0]);
        }
    }
    for (index, character) in text.chars().enumerate() {
        let origin = 1 + index * 6;
        if origin + 5 >= width {
            break;
        }
        for (column, bits) in glyph(character.to_ascii_uppercase()).into_iter().enumerate() {
            for row in 0..7 {
                if bits & (1 << row) != 0 {
                    put(rgba, origin + column, 1 + row, [0, 0xff, 0x40]);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_jit_verdict_off_wie_log_lines() {
        note_log_line("Create thread: 1");
        assert!(matches!(*lock(&JIT), Jit::Unknown));
        note_log_line("ARM AOT preparation failed; using interpreter");
        assert!(matches!(*lock(&JIT), Jit::Failed));
        note_log_line("ARM AOT installed");
        assert!(matches!(*lock(&JIT), Jit::Installed));
    }

    #[test]
    fn draws_only_inside_the_buffer() {
        let (width, height) = (40u32, 12u32);
        let mut rgba = vec![0xffu8; (width * height * 4) as usize];
        draw(&mut rgba, width, height, "JIT ON 9.9F");
        // The band is blacked out and at least one glyph pixel is lit.
        assert_eq!(&rgba[0..3], &[0, 0, 0]);
        assert!(rgba.chunks(4).any(|pixel| pixel[1] == 0xff && pixel[0] == 0));
        // Rows below the band are untouched.
        let below = (10 * width) as usize * 4;
        assert_eq!(&rgba[below..below + 3], &[0xff, 0xff, 0xff]);
    }
}
