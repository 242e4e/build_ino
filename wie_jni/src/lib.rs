//! `libwie_jni.so` for the `net.wie.app` Android shell, built on upstream wie.
//!
//! The Java side (WieActivity/GameView/PadCfg) is unchanged; this crate only has to
//! honour the contract it expects from `net.wie.app.WieNative`:
//!
//! * `nativeInit(zip, filesDir) -> int` — 0 on success, otherwise see `nativeLastError`.
//! * `nativeTick()` — called ~60 times per second on the game thread, followed by
//!   draining `nativePollOut()` until it returns null.
//! * `nativeKey(code, down)` — 1..4 = UP DOWN LEFT RIGHT, 5 OK, 6 LSK, 7 RSK, 8 CLEAR,
//!   9 CALL, 10..19 = NUM0..NUM9, 20 STAR, 21 HASH, 22 HANGUP.
//! * `nativePollOut() -> byte[]` — first byte is a tag:
//!   0 = play SFX (rest: SMAF bytes), 1 = play BGM loop (rest: SMAF bytes),
//!   2 = stop (`[2, 1]` BGM, `[2, 0]` SFX), 3 = vibrate (u32 LE milliseconds),
//!   4 = PCM (u32 LE sample rate, then i16 LE mono samples), 5 = volume (0..100).
//! * `nativeSetSurface(Surface)` — target for frames; null when destroyed.
//! * Saves live in `filesDir/wie_fs_v1` and `filesDir/wie_db_v1` (see `store`).

pub mod aot_cache;
pub mod store;
pub mod window;

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet, VecDeque},
    panic::{AssertUnwindSafe, catch_unwind},
    path::Path,
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::{Duration, Instant as StdInstant, SystemTime, UNIX_EPOCH},
};

use jni::{
    JNIEnv,
    objects::{JByteArray, JClass, JObject, JString},
    sys::{jboolean, jbyteArray, jint},
};
use wie_backend::{
    AudioCommand, AudioEventData, AudioHandle, Emulator, Event, Filesystem, Font, Instant, KeyCode, Options, Platform, Screen,
    canvas::Image,
};

use crate::{store::Store, window::Frame};

const KEY_REPEAT: Duration = Duration::from_millis(100);

static WIDTH: AtomicU32 = AtomicU32::new(240);
static HEIGHT: AtomicU32 = AtomicU32::new(320);
static OUT: Mutex<VecDeque<Vec<u8>>> = Mutex::new(VecDeque::new());
static KEYS: Mutex<VecDeque<(KeyCode, bool)>> = Mutex::new(VecDeque::new());
static LAST_ERROR: Mutex<String> = Mutex::new(String::new());
static EXITED: AtomicBool = AtomicBool::new(false);

fn lock<T>(mutex: &'static Mutex<T>) -> std::sync::MutexGuard<'static, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

fn push_out(message: Vec<u8>) {
    let mut out = lock(&OUT);
    // Never let an unread queue grow without bound (e.g. while paused).
    if out.len() > 512 {
        out.pop_front();
    }
    out.push_back(message);
}

fn set_error(message: impl Into<String>) {
    let message = message.into();
    tracing::error!("{message}");
    *lock(&LAST_ERROR) = message;
}

// ---------------------------------------------------------------- platform

struct JniScreen;

impl Screen for JniScreen {
    fn resize(&self, width: u32, height: u32) -> wie_util::Result<()> {
        WIDTH.store(width, Ordering::Relaxed);
        HEIGHT.store(height, Ordering::Relaxed);
        Ok(())
    }

    fn request_redraw(&self) -> wie_util::Result<()> {
        REDRAW.store(true, Ordering::Relaxed);
        Ok(())
    }

    fn paint(&self, image: &dyn Image) {
        let (width, height) = (image.width(), image.height());
        let mut rgba = Vec::with_capacity((width * height * 4) as usize);
        for color in image.colors() {
            rgba.extend_from_slice(&[color.r, color.g, color.b, 0xff]);
        }
        rgba.resize((width * height * 4) as usize, 0);
        window::present(Frame { width, height, rgba });
    }

    fn width(&self) -> u32 {
        WIDTH.load(Ordering::Relaxed)
    }

    fn height(&self) -> u32 {
        HEIGHT.load(Ordering::Relaxed)
    }
}

static BGM_HANDLES: Mutex<Option<HashSet<AudioHandle>>> = Mutex::new(None);

struct JniAudioSink;

impl wie_backend::AudioSink for JniAudioSink {
    fn send(&self, command: AudioCommand) {
        match command {
            AudioCommand::Play { handle, sequence, repeat } => {
                {
                    let mut bgm = lock(&BGM_HANDLES);
                    let bgm = bgm.get_or_insert_with(HashSet::new);
                    if repeat {
                        bgm.insert(handle);
                    } else {
                        bgm.remove(&handle);
                    }
                }

                if let Some(source) = &sequence.source {
                    let mut message = Vec::with_capacity(source.len() + 1);
                    message.push(if repeat { 1 } else { 0 });
                    message.extend_from_slice(source);
                    push_out(message);
                    return;
                }

                // No SMAF source: forward any raw PCM the sequence carries.
                for event in &sequence.events {
                    if let AudioEventData::Wave {
                        channels,
                        sampling_rate,
                        samples,
                    } = &event.data
                    {
                        let channels = (*channels).max(1) as usize;
                        let mut message = Vec::with_capacity(5 + samples.len() / channels * 2);
                        message.push(4);
                        message.extend_from_slice(&sampling_rate.to_le_bytes());
                        for frame in samples.chunks(channels) {
                            let mixed = frame.iter().map(|&s| s as i32).sum::<i32>() / frame.len() as i32;
                            message.extend_from_slice(&(mixed as i16).to_le_bytes());
                        }
                        push_out(message);
                    }
                }
            }
            AudioCommand::Stop { handle } => {
                let is_bgm = lock(&BGM_HANDLES).as_ref().is_some_and(|bgm| bgm.contains(&handle));
                push_out(vec![2, if is_bgm { 1 } else { 0 }]);
            }
        }
    }
}

struct JniPlatform {
    font: Font,
    screen: JniScreen,
    store: Store,
}

impl Platform for JniPlatform {
    fn font(&self) -> &Font {
        &self.font
    }

    fn screen(&self) -> &dyn Screen {
        &self.screen
    }

    fn now(&self) -> Instant {
        let millis = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis();
        Instant::from_epoch_millis(millis as _)
    }

    fn database_repository(&self) -> &dyn wie_backend::DatabaseRepository {
        &self.store
    }

    fn filesystem(&self) -> &dyn Filesystem {
        &self.store
    }

    fn audio_sink(&self) -> Box<dyn wie_backend::AudioSink> {
        Box::new(JniAudioSink)
    }

    fn write_stdout(&self, buf: &[u8]) {
        tracing::info!("[app] {}", String::from_utf8_lossy(buf).trim_end());
    }

    fn write_stderr(&self, buf: &[u8]) {
        tracing::warn!("[app] {}", String::from_utf8_lossy(buf).trim_end());
    }

    fn exit(&self) {
        tracing::info!("[app] exit requested");
        EXITED.store(true, Ordering::Relaxed);
    }

    fn vibrate(&self, duration_ms: u64, _intensity: u8) {
        let mut message = vec![3];
        message.extend_from_slice(&(duration_ms.min(u32::MAX as u64) as u32).to_le_bytes());
        push_out(message);
    }
}

// ---------------------------------------------------------------- runtime

struct Runtime {
    emulator: Box<dyn Emulator>,
    store: Store,
    ready: bool,
    failed: bool,
    held: HashMap<KeyCode, StdInstant>,
}

thread_local! {
    // nativeInit and nativeTick both run on the game thread.
    static RUNTIME: RefCell<Option<Runtime>> = const { RefCell::new(None) };
}

static REDRAW: AtomicBool = AtomicBool::new(false);

fn key_from_code(code: i32) -> Option<KeyCode> {
    Some(match code {
        1 => KeyCode::UP,
        2 => KeyCode::DOWN,
        3 => KeyCode::LEFT,
        4 => KeyCode::RIGHT,
        5 => KeyCode::OK,
        6 => KeyCode::LEFT_SOFT_KEY,
        7 => KeyCode::RIGHT_SOFT_KEY,
        8 => KeyCode::CLEAR,
        9 => KeyCode::CALL,
        10 => KeyCode::NUM0,
        11 => KeyCode::NUM1,
        12 => KeyCode::NUM2,
        13 => KeyCode::NUM3,
        14 => KeyCode::NUM4,
        15 => KeyCode::NUM5,
        16 => KeyCode::NUM6,
        17 => KeyCode::NUM7,
        18 => KeyCode::NUM8,
        19 => KeyCode::NUM9,
        20 => KeyCode::STAR,
        21 => KeyCode::HASH,
        22 => KeyCode::HANGUP,
        _ => return None,
    })
}

/// Starts the game in `zip` (a KTF/LGT/SKT archive), persisting saves under `data_dir`.
pub fn init(zip: Vec<u8>, data_dir: &Path) -> anyhow::Result<()> {
    logging::init();
    lock(&OUT).clear();
    lock(&KEYS).clear();
    EXITED.store(false, Ordering::Relaxed);

    let store = Store::open(data_dir);
    let platform = Box::new(JniPlatform {
        font: Font::try_from_static(include_bytes!("../../wie/assets/neodgm.ttf"))?,
        screen: JniScreen,
        store: store.clone(),
    });

    // Without a CompiledExecutor, wie falls back to stepping the ARM interpreter one
    // instruction at a time, which is far too slow for this phone's ARMv8 core. The
    // official wie-app frontend instead JIT-compiles hot code via wie-core-arm-native;
    // we do the same, with compiled regions cached to disk so repeat launches skip
    // recompilation.
    let cache = Box::new(aot_cache::FileCache::new(data_dir.join("aot_cache")));
    let options = Options {
        enable_gdbserver: false,
        aot: Some(Box::new(wie_core_arm_native::NativeExecutor::new(Some(cache)))),
        profile: None,
    };
    let emulator = wie::load_emulator("game.zip", zip, platform, options)?;
    tracing::info!("[init] emulator created, screen {}x{}", WIDTH.load(Ordering::Relaxed), HEIGHT.load(Ordering::Relaxed));

    RUNTIME.with_borrow_mut(|runtime| {
        *runtime = Some(Runtime {
            emulator,
            store,
            ready: false,
            failed: false,
            held: HashMap::new(),
        })
    });
    Ok(())
}

/// Advances the emulator by one frame. Returns false once it has stopped.
pub fn tick() -> bool {
    RUNTIME.with_borrow_mut(|runtime| {
        let Some(runtime) = runtime.as_mut() else { return false };
        if runtime.failed || EXITED.load(Ordering::Relaxed) {
            runtime.store.flush(true);
            return false;
        }

        let now = StdInstant::now();
        let keys: Vec<(KeyCode, bool)> = lock(&KEYS).drain(..).collect();
        for (key, down) in keys {
            if down {
                if runtime.ready && !runtime.held.contains_key(&key) {
                    runtime.emulator.handle_event(Event::Keydown(key));
                    runtime.held.insert(key, now);
                }
            } else if runtime.held.remove(&key).is_some() {
                runtime.emulator.handle_event(Event::Keyup(key));
            }
        }
        for (key, since) in runtime.held.iter_mut() {
            if now.duration_since(*since) > KEY_REPEAT {
                runtime.emulator.handle_event(Event::Keyrepeat(*key));
                *since = now;
            }
        }

        if REDRAW.swap(false, Ordering::Relaxed) {
            runtime.emulator.handle_event(Event::Redraw);
        }

        if let Err(error) = runtime.emulator.tick() {
            runtime.failed = true;
            set_error(format!("emulator stopped: {error}"));
            runtime.store.flush(true);
            return false;
        }

        if !runtime.ready && !runtime.emulator.is_preparing() {
            runtime.ready = true;
            tracing::info!("[tick] emulator ready");
        }

        runtime.store.flush(false);
        true
    })
}

pub fn key(code: i32, down: bool) {
    if let Some(key) = key_from_code(code) {
        lock(&KEYS).push_back((key, down));
    }
}

pub fn poll_out() -> Option<Vec<u8>> {
    lock(&OUT).pop_front()
}

pub fn last_error() -> String {
    lock(&LAST_ERROR).clone()
}

pub fn screen_size() -> (u32, u32) {
    (WIDTH.load(Ordering::Relaxed), HEIGHT.load(Ordering::Relaxed))
}

fn guarded<R>(fallback: R, f: impl FnOnce() -> R) -> R {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(value) => value,
        Err(panic) => {
            let message = panic
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".into());
            set_error(format!("panic: {message}"));
            RUNTIME.with_borrow_mut(|runtime| {
                if let Some(runtime) = runtime.as_mut() {
                    runtime.failed = true;
                }
            });
            fallback
        }
    }
}

// ---------------------------------------------------------------- JNI

fn byte_array(env: &mut JNIEnv<'_>, bytes: Option<Vec<u8>>) -> jbyteArray {
    match bytes {
        Some(bytes) => env
            .byte_array_from_slice(&bytes)
            .map(JByteArray::into_raw)
            .unwrap_or(std::ptr::null_mut()),
        None => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_net_wie_app_WieNative_nativeInit<'l>(mut env: JNIEnv<'l>, _class: JClass<'l>, zip: JByteArray<'l>, dir: JString<'l>) -> jint {
    let zip = match env.convert_byte_array(&zip) {
        Ok(zip) => zip,
        Err(error) => {
            set_error(format!("cannot read game data: {error}"));
            return 1;
        }
    };
    let dir: String = match env.get_string(&dir) {
        Ok(dir) => dir.into(),
        Err(error) => {
            set_error(format!("cannot read files dir: {error}"));
            return 1;
        }
    };

    guarded(1, || match init(zip, Path::new(&dir)) {
        Ok(()) => 0,
        Err(error) => {
            set_error(format!("{error:#}"));
            1
        }
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_net_wie_app_WieNative_nativeTick<'l>(_env: JNIEnv<'l>, _class: JClass<'l>) {
    guarded((), || {
        tick();
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_net_wie_app_WieNative_nativeKey<'l>(_env: JNIEnv<'l>, _class: JClass<'l>, code: jint, down: jboolean) {
    key(code, down != 0);
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_net_wie_app_WieNative_nativeSetSurface<'l>(env: JNIEnv<'l>, _class: JClass<'l>, surface: JObject<'l>) {
    guarded((), || {
        window::set_surface(&env, &surface);
        REDRAW.store(true, Ordering::Relaxed);
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_net_wie_app_WieNative_nativePollOut<'l>(mut env: JNIEnv<'l>, _class: JClass<'l>) -> jbyteArray {
    let message = poll_out();
    byte_array(&mut env, message)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_net_wie_app_WieNative_nativeLastError<'l>(mut env: JNIEnv<'l>, _class: JClass<'l>) -> jbyteArray {
    let error = last_error();
    byte_array(&mut env, Some(error.into_bytes()))
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_net_wie_app_WieNative_nativeProfileText<'l>(_env: JNIEnv<'l>, _class: JClass<'l>, _reset: jboolean) -> jbyteArray {
    std::ptr::null_mut()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_net_wie_app_WieNative_nativeWidth<'l>(_env: JNIEnv<'l>, _class: JClass<'l>) -> jint {
    WIDTH.load(Ordering::Relaxed) as jint
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_net_wie_app_WieNative_nativeHeight<'l>(_env: JNIEnv<'l>, _class: JClass<'l>) -> jint {
    HEIGHT.load(Ordering::Relaxed) as jint
}

// ---------------------------------------------------------------- logging

mod logging {
    use std::{io, sync::Once};

    static INIT: Once = Once::new();

    #[cfg(target_os = "android")]
    mod sink {
        use std::ffi::{CString, c_char, c_int};

        #[link(name = "log")]
        unsafe extern "C" {
            fn __android_log_write(prio: c_int, tag: *const c_char, text: *const c_char) -> c_int;
        }

        pub fn write(line: &str) {
            const ANDROID_LOG_INFO: c_int = 4;
            let tag = c"wie";
            if let Ok(text) = CString::new(line.replace('\0', " ")) {
                unsafe { __android_log_write(ANDROID_LOG_INFO, tag.as_ptr(), text.as_ptr()) };
            }
        }
    }

    #[cfg(not(target_os = "android"))]
    mod sink {
        pub fn write(line: &str) {
            eprintln!("{line}");
        }
    }

    struct LineWriter(Vec<u8>);

    impl io::Write for LineWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Drop for LineWriter {
        fn drop(&mut self) {
            let text = String::from_utf8_lossy(&self.0);
            for line in text.lines().filter(|line| !line.is_empty()) {
                sink::write(line);
            }
        }
    }

    pub fn init() {
        INIT.call_once(|| {
            let _ = tracing_subscriber::fmt()
                .with_max_level(match std::env::var("WIE_LOG").as_deref() {
                    Ok("trace") => tracing::Level::TRACE,
                    Ok("debug") => tracing::Level::DEBUG,
                    _ => tracing::Level::INFO,
                })
                .with_ansi(false)
                .without_time()
                .with_target(false)
                .with_writer(|| LineWriter(Vec::new()))
                .try_init();
        });
    }
}
