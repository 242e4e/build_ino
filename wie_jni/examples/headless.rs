//! Host-side smoke test: boots a game zip through the same code path as the JNI
//! entry points, feeds a scripted key sequence, and dumps frames as PNG.
//!
//! usage: headless <game.zip> <out_dir> <seconds> [KEY@sec ...]   e.g. OK@6 OK@8

use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::BufWriter,
    path::Path,
    time::{Duration, Instant},
};

fn key_code(name: &str) -> i32 {
    match name {
        "UP" => 1,
        "DOWN" => 2,
        "LEFT" => 3,
        "RIGHT" => 4,
        "OK" => 5,
        "LSK" => 6,
        "RSK" => 7,
        "CLEAR" => 8,
        "STAR" => 20,
        "HASH" => 21,
        n if n.len() == 1 && n.as_bytes()[0].is_ascii_digit() => 10 + (n.as_bytes()[0] - b'0') as i32,
        _ => panic!("unknown key {name}"),
    }
}

fn save_png(path: &Path, frame: &wie_jni::window::Frame) {
    let file = BufWriter::new(File::create(path).unwrap());
    let mut encoder = png::Encoder::new(file, frame.width, frame.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header().unwrap().write_image_data(&frame.rgba).unwrap();
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let zip = fs::read(&args[1]).expect("game zip");
    let out = Path::new(&args[2]);
    let seconds: f64 = args[3].parse().unwrap();
    let mut script: Vec<(f64, i32)> = args[4..]
        .iter()
        .map(|item| {
            let (key, at) = item.split_once('@').unwrap();
            (at.parse().unwrap(), key_code(key))
        })
        .collect();
    script.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());

    fs::create_dir_all(out.join("data")).unwrap();
    wie_jni::init(zip, &out.join("data")).unwrap_or_else(|e| panic!("init failed: {e:#}"));
    println!("screen {:?}", wie_jni::screen_size());

    let start = Instant::now();
    let mut next_dump = 1.0;
    let mut tags: BTreeMap<u8, usize> = BTreeMap::new();
    let mut pending_up: Vec<(f64, i32)> = Vec::new();
    let mut ticks = 0u64;

    loop {
        let t = start.elapsed().as_secs_f64();
        if t > seconds {
            break;
        }
        while let Some(&(at, code)) = script.first() {
            if at > t {
                break;
            }
            script.remove(0);
            wie_jni::key(code, true);
            pending_up.push((at + 0.15, code));
            println!("[{t:6.2}] key down {code}");
        }
        pending_up.retain(|&(at, code)| {
            if at <= t {
                wie_jni::key(code, false);
                false
            } else {
                true
            }
        });

        if !wie_jni::tick() {
            println!("[{t:6.2}] stopped: {}", wie_jni::last_error());
            break;
        }
        ticks += 1;
        while let Some(message) = wie_jni::poll_out() {
            *tags.entry(message[0]).or_default() += 1;
        }

        if t >= next_dump {
            if let Some(frame) = wie_jni::window::last_frame() {
                save_png(&out.join(format!("frame_{:03}.png", next_dump as u32)), &frame);
            }
            next_dump += 1.0;
        }

        let target = Duration::from_secs_f64(ticks as f64 / 60.0);
        if let Some(wait) = target.checked_sub(start.elapsed()) {
            std::thread::sleep(wait);
        }
    }

    println!("ticks {ticks}, out tags {tags:?}, error '{}'", wie_jni::last_error());
    if let Some(frame) = wie_jni::window::last_frame() {
        save_png(&out.join("frame_last.png"), &frame);
    }
}
