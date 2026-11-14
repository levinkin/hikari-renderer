mod behavior;
mod codec;
mod engine_ffi;
mod settings_tui;
mod vtube;

use behavior::Behavior;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::ffi::{c_void, CStr, CString};
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy)]
enum Lane {
    Shared = 0,
    Render = 1,
    RenderSplit = 2,
}

type Frames = (Vec<((usize, usize), Vec<u8>)>, [u8; 4]);

enum Job {
    Tick { dt_ms: f32, sizes: Vec<(usize, usize)>, reply: Sender<Frames> },
    Step { owner: usize, dt_ms: f32, mouth: f32, size: (usize, usize), reply: Sender<(Vec<u8>, [u8; 4])> },
    React { kind: String, reply: Sender<Value> },
    Wake,
    Background { bytes: Option<Arc<Vec<u8>>>, content_type: String, reply: Sender<i32> },
    Switch { scene: Scene, reply: Sender<Scene> },
}

#[repr(C)]
struct KEvent {
    ident: usize,
    filter: i16,
    flags: u16,
    fflags: u32,
    data: isize,
    udata: *mut c_void,
}

#[repr(C)]
struct TimeSpec {
    tv_sec: i64,
    tv_nsec: i64,
}

extern "C" {
    fn kqueue() -> i32;
    fn kevent(kq: i32, changelist: *const KEvent, nchanges: i32, eventlist: *mut KEvent, nevents: i32, timeout: *const TimeSpec) -> i32;
}

const SPLIT_SIZE: (usize, usize) = (320, 240);
const MOUTHS: usize = 256;
const TRANSITION_SECONDS_MAX: f64 = 3.0;
const TRANSITION_SECONDS_DEFAULT: f64 = 0.5;
const WINDOW_TURN_DEGREES: f32 = 90.0;
const PHYSICS_DEFAULT: [f32; 4] = [50.0, 0.0, 90.0, 0.0];
const MODEL_FRAMING: [f32; 4] = [f32::NAN; 4];
const TARGET_IDLE: Duration = Duration::from_secs(10);
const WAKE_MS: f32 = 1000.0;
const CODECS: [&str; 6] = ["jpeg", "pal256", "rgb565", "split", "qoi", "mjpeg"];
const JPEG: u8 = 0;
const MJPEG: u8 = 5;

struct App {
    jobs: Sender<Job>,
    timer_queue: i32,
    ready: AtomicBool,
    models: BTreeMap<String, PathBuf>,
    model: Mutex<Model>,
    size: (usize, usize),
    idle: Mutex<IdleSettings>,
    broken: Mutex<Vec<String>>,
    bg_lock: Mutex<()>,
    settings_lock: Mutex<()>,
    bg_dir: PathBuf,
    shipped: Vec<(String, PathBuf)>,
    default_actions: Vec<(String, PathBuf)>,
    views: Mutex<Views>,
    window_background: Mutex<Option<(Option<Arc<Vec<u8>>>, String)>>,
    mouths: Mutex<Mouths>,
    state: Mutex<ModelState>,
    asleep: AtomicBool,
    head_lead_ms: AtomicU64,
    params: Mutex<Vec<(String, i32, f32)>>,
    physics: Mutex<Option<[f32; 4]>>,
    settings_physics: Mutex<serde_json::Map<String, Value>>,
    transition_seconds: AtomicU64,
    shared: Mutex<Shared>,
    picture: (Mutex<Picture>, Condvar),
    lane_locks: [Mutex<()>; 3],
    rendering: [AtomicBool; 3],
}

#[derive(Clone, PartialEq)]
struct ModelState {
    face: String,
    pose: String,
    face_given: bool,
    generation: u64,
}

struct Mouths {
    owner: usize,
    t0: Instant,
    fps: f64,
    ring: [(usize, f32); MOUTHS],
}

impl Mouths {
    fn mouth_now(&self, now: Instant) -> f32 {
        let at = (now.saturating_duration_since(self.t0).as_secs_f64() * self.fps) as usize;
        let (frame, mouth) = self.ring[at % MOUTHS];
        if frame == at { mouth } else { -1.0 }
    }
}

struct Shared {
    engine: usize,
    name: String,
    window: String,
    pending: Vec<String>,
    bg_clock_ms: f64,
}

struct Picture {
    seq: u64,
    head: [u8; 4],
    frames: HashMap<((usize, usize), u8), Arc<Vec<u8>>>,
    wants: Vec<((usize, usize), u8, f64)>,
    window_fps: f64,
}

#[derive(Clone)]
struct Model {
    name: String,
    states: Vec<String>,
    actions: Vec<String>,
    framing: Option<[f32; 4]>,
    params: Vec<(String, f32, f32, i32)>,
    physics: [f32; 4],
    own_physics: Option<(f32, f32)>,
}

struct Scene {
    model: Model,
    lanes: Vec<LaneState>,
    targets: HashMap<(usize, usize), (*mut engine_ffi::Engine, Instant)>,
    asleep: bool,
    stepped_since_tick: bool,
}

unsafe impl Send for Scene {}

struct Views {
    live: HashMap<String, [f32; 4]>,
    saved: HashMap<String, [f32; 4]>,
    path: PathBuf,
}

struct LaneState {
    width: usize,
    height: usize,
    engine: *mut engine_ffi::Engine,
    behavior: Behavior,
    framing: Option<[f32; 4]>,
    view: [f32; 4],
    physics: [f32; 4],
    idle_generation: u64,
    state_generation: u64,
}

struct IdleSettings {
    actions: Vec<String>,
    every: Value,
    face: String,
    idle_loop: String,
    generation: u64,
    path: PathBuf,
}

struct WindowLoop<'a> {
    app: &'a App,
    name: String,
    size: (usize, usize),
    state: LaneState,
    window: *mut engine_ffi::Window,
    frame_seconds: f32,
    last: Instant,
    second: Instant,
    frames: u32,
    mouth_frames: u32,
    mouth_max: f32,
    width: i32,
    height: i32,
    share: bool,
}

struct Request {
    method: String,
    path: String,
    query: Vec<(String, String)>,
    headers: Vec<(String, String)>,
}

struct Body {
    reader: BufReader<TcpStream>,
    remaining: u64,
    chunked: bool,
    done: bool,
}

struct Inbound {
    pcm: Vec<u8>,
    base: usize,
    cap: usize,
    channels: usize,
    partial: Vec<u8>,
    ended: bool,
    closed: bool,
    t0: Option<Instant>,
}

impl Read for Body {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.done {
            return Ok(0);
        }
        if self.remaining == 0 {
            if !self.chunked {
                self.done = true;
                return Ok(0);
            }
            let mut line = String::new();
            if self.reader.read_line(&mut line)? == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            let size_text = line.split(';').next().unwrap_or_default().trim();
            let size = u64::from_str_radix(size_text, 16).map_err(io::Error::other)?;
            if size == 0 {
                loop {
                    line.clear();
                    if self.reader.read_line(&mut line)? == 0 || line.trim().is_empty() {
                        break;
                    }
                }
                self.done = true;
                return Ok(0);
            }
            self.remaining = size;
        }
        let want = buf.len().min(self.remaining as usize);
        let n = self.reader.read(&mut buf[..want])?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        self.remaining -= n as u64;
        if self.chunked && self.remaining == 0 {
            let mut crlf = String::new();
            self.reader.read_line(&mut crlf)?;
        }
        Ok(n)
    }
}

fn param(req: &Request, name: &str, default: &str) -> String {
    for (key, value) in &req.query {
        if key == name {
            if value.is_empty() {
                break;
            }
            return value.clone();
        }
    }
    default.to_string()
}

fn single_param<'a>(req: &'a Request, name: &str) -> Result<Option<&'a str>, String> {
    let mut found = None;
    for (key, value) in &req.query {
        if key != name {
            continue;
        }
        if found.is_some() {
            return Err(format!("hikari: {} given more than once\n", name));
        }
        found = Some(value.as_str());
    }
    Ok(found)
}

fn bool_param(req: &Request, name: &str) -> Result<Option<bool>, String> {
    match single_param(req, name)? {
        None => Ok(None),
        Some("true") => Ok(Some(true)),
        Some("false") => Ok(Some(false)),
        Some(_) => Err(format!("hikari: {} must be true or false\n", name)),
    }
}

fn header(req: &Request, name: &str) -> String {
    for (key, value) in &req.headers {
        if key == name {
            return value.clone();
        }
    }
    String::new()
}

fn js_number(text: &str) -> f64 {
    let trimmed = text.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}').to_ascii_lowercase();
    if trimmed.is_empty() {
        return 0.0;
    }
    for (prefix, radix) in [("0x", 16), ("0b", 2), ("0o", 8)] {
        if let Some(digits) = trimmed.strip_prefix(prefix) {
            if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
                return f64::NAN;
            }
            return u128::from_str_radix(digits, radix).map_or(f64::INFINITY, |v| v as f64);
        }
    }
    trimmed.parse().unwrap_or(f64::NAN)
}

fn iso_now() -> String {
    let ms = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64;
    let day_ms = ms.rem_euclid(86_400_000);
    let z = ms.div_euclid(86_400_000) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + if month <= 2 { 1 } else { 0 };
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z", year, month, day, day_ms / 3_600_000, day_ms / 60_000 % 60, day_ms / 1000 % 60, day_ms % 1000)
}

const STREAM_HEAD: &[u8] = b"HTTP/1.1 200 OK\r\ncontent-type: application/octet-stream\r\nConnection: close\r\nTransfer-Encoding: chunked\r\n\r\n";
const MJPEG_HEAD: &[u8] = b"HTTP/1.1 200 OK\r\ncontent-type: multipart/x-mixed-replace; boundary=frame\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n";

fn read_object(body: Body, out: &mut TcpStream, route: &str) -> Option<serde_json::Map<String, Value>> {
    let mut text = String::new();
    match body.take(65536).read_to_string(&mut text).ok().and_then(|_| serde_json::from_str(&text).ok()) {
        Some(Value::Object(fields)) => Some(fields),
        _ => {
            send_body(out, 400, "text/plain", format!("hikari: {} needs a JSON object\n", route).as_bytes());
            None
        }
    }
}

fn send_body(out: &mut TcpStream, status: u16, content_type: &str, body: &[u8]) {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        413 => "Payload Too Large",
        503 => "Service Unavailable",
        _ => "Internal Server Error",
    };
    let mut response = format!("HTTP/1.1 {} {}\r\n", status, reason).into_bytes();
    if !content_type.is_empty() {
        response.extend(format!("content-type: {}\r\n", content_type).into_bytes());
    }
    response.extend(format!("Content-Length: {}\r\nConnection: close\r\n\r\n", body.len()).into_bytes());
    response.extend_from_slice(body);
    out.write_all(&response).ok();
}

fn write_frame(out: &mut TcpStream, pts: f64, head: &[u8], codec_id: u8, data: &[u8]) -> bool {
    let mut chunk = Vec::with_capacity(data.len() + 80);
    if codec_id == MJPEG {
        write!(chunk, "--frame\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n", data.len()).unwrap();
        chunk.extend_from_slice(data);
        chunk.extend_from_slice(b"\r\n");
        return out.write_all(&chunk).is_ok();
    }
    write!(chunk, "{:x}\r\n", data.len() + head.len() + 9).unwrap();
    chunk.extend((pts as u32).to_be_bytes());
    chunk.extend((data.len() as u32 + 1).to_be_bytes());
    chunk.extend_from_slice(head);
    chunk.push(codec_id);
    chunk.extend_from_slice(data);
    chunk.extend_from_slice(b"\r\n");
    out.write_all(&chunk).is_ok()
}

fn window_mouth(pcm: &[u8], start: usize, end: usize) -> f32 {
    if end <= start {
        return 0.0;
    }
    let mut sum = 0.0f64;
    for i in start..end {
        let v = i16::from_le_bytes([pcm[i * 2], pcm[i * 2 + 1]]) as f64;
        sum += v * v;
    }
    let rms = (sum / (end - start) as f64).sqrt();
    (0.45 * ((rms - 500.0) / 9000.0).clamp(0.0, 1.0)) as f32
}

fn read_inbound(mut body: io::Take<Body>, shared: Arc<(Mutex<Inbound>, Condvar)>) {
    let mut buf = vec![0u8; 65536];
    loop {
        let mut guard = shared.0.lock().unwrap();
        while guard.pcm.len() / 2 >= guard.cap && !guard.closed {
            guard = shared.1.wait(guard).unwrap();
        }
        if guard.closed {
            return;
        }
        drop(guard);
        match body.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let mut guard = shared.0.lock().unwrap();
                let state = &mut *guard;
                if state.t0.is_none() {
                    state.t0 = Some(Instant::now());
                }
                let channels = state.channels;
                state.partial.extend_from_slice(&buf[..n]);
                let whole = state.partial.len() / (channels * 2) * channels * 2;
                for chunk in state.partial[..whole].chunks_exact(channels * 2) {
                    let sum: i32 = chunk.chunks_exact(2).map(|s| i16::from_le_bytes([s[0], s[1]]) as i32).sum();
                    state.pcm.extend_from_slice(&((sum / channels as i32) as i16).to_le_bytes());
                }
                state.partial.drain(..whole);
                shared.1.notify_all();
            }
            Err(_) => {
                shared.0.lock().unwrap().closed = true;
                shared.1.notify_all();
                return;
            }
        }
    }
    shared.0.lock().unwrap().ended = true;
    shared.1.notify_all();
}

fn read_wav_header(body: &mut Body) -> Result<(f64, usize, u64), String> {
    let mut riff = [0u8; 12];
    if body.read_exact(&mut riff).is_err() || &riff[..4] != b"RIFF" || &riff[8..] != b"WAVE" {
        return Err(format!("hikari: render body must be a RIFF/WAVE file, got {:?}", String::from_utf8_lossy(&riff[..4])));
    }
    let mut format = None;
    let mut header_bytes = riff.len() as u64;
    loop {
        let mut head = [0u8; 8];
        if body.read_exact(&mut head).is_err() {
            return Err("hikari: render body must be a wav with a data chunk, got none".to_string());
        }
        let size = u32::from_le_bytes([head[4], head[5], head[6], head[7]]);
        if &head[..4] == b"data" {
            let Some((rate, channels)) = format else {
                return Err("hikari: render body must have a fmt chunk before data, got data first".to_string());
            };
            let limit = if size == 0 || size == u32::MAX { u64::MAX } else { size as u64 };
            return Ok((rate, channels, limit));
        }
        let padded = size as u64 + (size & 1) as u64;
        header_bytes += head.len() as u64 + padded;
        if header_bytes > 65536 {
            return Err("hikari: wav header too large".to_string());
        }
        let is_fmt = &head[..4] == b"fmt ";
        if is_fmt && size < 16 {
            return Err(format!("hikari: render body fmt chunk must be 16 bytes or more, got {}", size));
        }
        let mut chunk = [0u8; 16];
        let skip = if is_fmt { padded - 16 } else { padded };
        if (is_fmt && body.read_exact(&mut chunk).is_err()) || !matches!(io::copy(&mut body.by_ref().take(skip), &mut io::sink()), Ok(n) if n == skip) {
            return Err(format!("hikari: render body must be a complete wav, got a cut {} chunk", String::from_utf8_lossy(&head[..4])));
        }
        if !is_fmt {
            continue;
        }
        let tag = u16::from_le_bytes([chunk[0], chunk[1]]);
        let channels = u16::from_le_bytes([chunk[2], chunk[3]]);
        let rate = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
        let bits = u16::from_le_bytes([chunk[14], chunk[15]]);
        if tag != 1 {
            return Err(format!("hikari: render body must be pcm wav (format 1), got format {}", tag));
        }
        if bits != 16 {
            return Err(format!("hikari: render body must be 16-bit pcm wav, got {}-bit", bits));
        }
        if !(1..=2).contains(&channels) {
            return Err(format!("hikari: render body must have 1 or 2 channels, got {}", channels));
        }
        if !(8000..=96000).contains(&rate) {
            return Err(format!("hikari: render body rate must be 8000-96000, got {}", rate));
        }
        format = Some((rate as f64, channels as usize));
    }
}

fn request_view(req: &Request) -> [f32; 4] {
    let mut view = ["zoom", "x", "y", "rot"].map(|name| js_number(&param(req, name, "nan")) as f32);
    if !(view[0] > 0.0) {
        view[0] = f32::NAN;
    }
    view
}

fn fps_param(req: &Request) -> Result<f64, String> {
    let value = single_param(req, "fps")?.unwrap_or("60");
    match value.parse::<u32>() {
        Ok(fps @ 1..=240) if value.bytes().all(|byte| byte.is_ascii_digit()) => Ok(fps as f64),
        _ => Err(format!("hikari: fps must be a whole number from 1 to 240, got {}\n", value)),
    }
}

fn size_param(req: &Request, default: (usize, usize)) -> Result<(usize, usize), String> {
    let Some(value) = single_param(req, "size")? else { return Ok(default) };
    let parsed = value.bytes().all(|b| b.is_ascii_digit() || b == b'x').then_some(value).and_then(|value| value.split_once('x').and_then(|(w, h)| Some((w.parse::<usize>().ok()?, h.parse::<usize>().ok()?))));
    match parsed {
        Some((w, h)) if (1..=3840).contains(&w) && (1..=2160).contains(&h) => Ok((w, h)),
        _ => Err(format!("hikari: size must be WxH up to 3840x2160, got {}\n", value)),
    }
}

fn handle_render(app: &App, out: &mut TcpStream, body: io::Take<Body>, fps: f64, (rate, channels): (f64, usize), size: (usize, usize), codec_name: &str, codec_id: u8) {
    let lane = if codec_name == "split" { Lane::RenderSplit } else { Lane::Render };
    let guard = app.lane_locks[lane as usize].lock().unwrap_or_else(PoisonError::into_inner);
    let shared = Arc::new((Mutex::new(Inbound { pcm: Vec::new(), base: 0, cap: (rate * 2.0) as usize, channels, partial: Vec::new(), ended: false, closed: false, t0: None }), Condvar::new()));
    let reader_shared = shared.clone();
    let reader = thread::spawn(move || read_inbound(body, reader_shared));
    app.rendering[lane as usize].store(true, Ordering::SeqCst);
    app.jobs.send(Job::Wake).unwrap();
    out.write_all(if codec_id == MJPEG { MJPEG_HEAD } else { STREAM_HEAD }).ok();
    let owner = Arc::as_ptr(&shared) as usize;
    let mut k = 0u64;
    let mut behind_max = 0.0f64;
    loop {
        let mut state = shared.0.lock().unwrap();
        if state.closed {
            break;
        }
        let samples = state.base + state.pcm.len() / 2;
        let start = (k as f64 * rate / fps).floor() as usize;
        let end = ((k + 1) as f64 * rate / fps).floor() as usize;
        if start >= samples && state.ended {
            break;
        }
        if end > samples && !state.ended {
            drop(shared.1.wait(state).unwrap());
            continue;
        }
        let used = end.min(samples);
        let mouth = window_mouth(&state.pcm, start - state.base, used - state.base);
        let drained = (used - state.base) * 2;
        state.pcm.drain(..drained);
        state.base = used;
        let t0 = state.t0.unwrap_or_else(Instant::now);
        drop(state);
        shared.1.notify_all();
        let mut mouths = app.mouths.lock().unwrap_or_else(PoisonError::into_inner);
        if k == 0 {
            mouths.owner = owner;
            mouths.t0 = t0;
            mouths.fps = fps;
            mouths.ring = [(usize::MAX, -1.0); MOUTHS];
        }
        if mouths.owner == owner {
            mouths.ring[k as usize % MOUTHS] = (k as usize, mouth);
        }
        drop(mouths);
        let pts = (k as f64 * 1000.0 / fps).floor();
        let (reply, frame) = channel();
        app.jobs.send(Job::Step { owner, dt_ms: (1000.0 / fps) as f32, mouth, size, reply }).unwrap();
        let (rgba, head) = frame.recv().unwrap();
        if !write_frame(out, pts, &head, codec_id, &codec::encode(&rgba, size.0, size.1, codec_name)) {
            break;
        }
        behind_max = behind_max.max(t0.elapsed().as_secs_f64() * 1000.0 - pts);
        k += 1;
    }
    let reading = {
        let mut state = shared.0.lock().unwrap();
        let reading = !state.ended && !state.closed;
        state.closed = true;
        reading
    };
    shared.1.notify_all();
    if reading {
        out.shutdown(Shutdown::Read).ok();
    }
    reader.join().ok();
    app.rendering[lane as usize].store(false, Ordering::SeqCst);
    let mut mouths = app.mouths.lock().unwrap_or_else(PoisonError::into_inner);
    if mouths.owner == owner {
        mouths.owner = 0;
    }
    drop(mouths);
    drop(guard);
    out.write_all(if codec_id == MJPEG { b"--frame--\r\n".as_slice() } else { b"0\r\n\r\n".as_slice() }).ok();
    println!("hikari: fps={} frames={} behind_ms_max={}", fps, k, behind_max.round());
}

fn put_view(app: &App, req: &Request, out: &mut TcpStream, save: bool) {
    let Model { name, framing, .. } = app.model.lock().unwrap_or_else(PoisonError::into_inner).clone();
    let request = request_view(req);
    let mut views = app.views.lock().unwrap_or_else(PoisonError::into_inner);
    let base = match views.live.get(&name).or(views.saved.get(&name)) {
        Some(view) => *view,
        None => resolve_view(MODEL_FRAMING, framing),
    };
    let view = resolve_view(request, Some(base));
    if view != base {
        views.live.insert(name.clone(), view);
        println!("hikari: view model={} zoom={} x={} y={} rot={}", name, view[0], view[1], view[2], view[3]);
    }
    if save {
        if let Err(line) = save_view(&mut views, &[(name, view)]) {
            send_body(out, 500, "text/plain", format!("{}\n", line).as_bytes());
            return;
        }
    }
    send_body(out, 204, "", b"");
}

fn physics_changes(fields: &serde_json::Map<String, Value>) -> Result<Vec<(usize, f32)>, String> {
    let mut entries = Vec::new();
    for (key, value) in fields {
        if key == "strength" {
            entries.push((key.clone(), value, 0));
            continue;
        }
        if key != "wind" && key != "gravity" {
            return Err(format!("hikari: unknown physics key {}", key));
        }
        let Some(inner) = value.as_object() else {
            return Err(format!("hikari: {} must be an object", key));
        };
        for (name, inner_value) in inner {
            let index = match (key.as_str(), name.as_str()) {
                ("wind", "strength") => 1,
                ("wind", "direction") => 2,
                ("gravity", "direction") => 3,
                _ => return Err(format!("hikari: unknown physics key {}.{}", key, name)),
            };
            entries.push((format!("{}.{}", key, name), inner_value, index));
        }
    }
    let mut changes = Vec::new();
    for (path, value, index) in entries {
        match value.as_f64() {
            Some(number) if (number as f32).is_finite() => changes.push((index, number as f32)),
            _ => return Err(format!("hikari: {} must be a number within float range", path)),
        }
    }
    Ok(changes)
}

fn effective_physics(own: Option<(f32, f32)>, changes: &[(usize, f32)]) -> [f32; 4] {
    let mut values = PHYSICS_DEFAULT;
    for &(index, value) in changes {
        values[index] = value;
    }
    if let Some((strength, wind)) = own {
        values[0] = strength;
        values[1] = wind;
    }
    values
}

fn describe_model(name: &str, setup: &vtube::VtubeSetup, shipped: &[(String, PathBuf)], default_actions: &[(String, PathBuf)], fallback: &serde_json::Map<String, Value>) -> Model {
    let mut states: Vec<String> = setup.states.iter().map(|(name, _)| name.clone()).chain(setup.state_files.iter().map(|(name, _)| name.clone())).chain(shipped.iter().map(|(name, _)| name.clone())).collect();
    states.sort();
    states.dedup();
    let mut actions = setup.actions.clone();
    actions.extend(default_actions.iter().map(|(name, _)| name.clone()));
    actions.sort();
    actions.dedup();
    Model { name: name.to_string(), states, actions, framing: setup.framing, params: Vec::new(), physics: effective_physics(setup.physics, &physics_changes(fallback).unwrap_or_default()), own_physics: setup.physics }
}

fn post_state(app: &App, req: &Request, out: &mut TcpStream) {
    let known = app.model.lock().unwrap_or_else(PoisonError::into_inner).states.clone();
    let asked: Vec<(String, String)> = req.query.iter().filter(|(key, _)| key == "face" || key == "pose").cloned().collect();
    if let Some((_, name)) = asked.iter().find(|(_, name)| name != "none" && !known.contains(name)) {
        send_body(out, 400, "text/plain", format!("hikari: unknown state {}\n", name).as_bytes());
        return;
    }
    let lead = req.query.iter().find(|(key, _)| key == "head_lead_ms").map(|(_, value)| value.parse::<u64>().ok().filter(|ms| *ms <= 1500).ok_or(value));
    if let Some(Err(value)) = lead {
        send_body(out, 400, "text/plain", format!("hikari: head_lead_ms must be an integer 0..1500, got {}\n", value).as_bytes());
        return;
    }
    if let Some(Ok(ms)) = lead {
        app.head_lead_ms.store(ms, Ordering::SeqCst);
    }
    let mut current = app.state.lock().unwrap_or_else(PoisonError::into_inner);
    let mut state = current.clone();
    let given = !asked.is_empty();
    for (key, name) in asked {
        if key == "face" {
            state.face = name;
            state.face_given = true;
        } else {
            state.pose = name;
        }
    }
    if state != *current {
        state.generation += 1;
    }
    if lead.is_none() || given {
        app.jobs.send(Job::Wake).unwrap();
    }
    let head_lead_ms = app.head_lead_ms.load(Ordering::SeqCst);
    println!("hikari: state face={} pose={} head_lead_ms={}", state.face, state.pose, head_lead_ms);
    *current = state.clone();
    drop(current);
    send_body(out, 200, "application/json", json!({ "face": state.face, "pose": state.pose, "asleep": app.asleep.load(Ordering::SeqCst), "head_lead_ms": head_lead_ms }).to_string().as_bytes());
}

fn post_params(app: &App, body: Body, out: &mut TcpStream) {
    let Some(fields) = read_object(body, out, "POST /params") else { return };
    let model = app.model.lock().unwrap_or_else(PoisonError::into_inner);
    let mut unknown = Vec::new();
    let mut matched = Vec::new();
    for (id, value) in &fields {
        let mut found = None;
        for row in &model.params {
            if &row.0 == id {
                found = Some(row);
                break;
            }
        }
        match found {
            Some(row) => matched.push((id, value, row)),
            None => unknown.push(id.as_str()),
        }
    }
    if !unknown.is_empty() {
        send_body(out, 400, "text/plain", format!("hikari: unknown params {}\n", unknown.join(",")).as_bytes());
        return;
    }
    for (id, value) in &fields {
        if !value.is_null() && !value.is_number() {
            send_body(out, 400, "text/plain", format!("hikari: {} must be a number or null\n", id).as_bytes());
            return;
        }
    }
    let mut params = app.params.lock().unwrap_or_else(PoisonError::into_inner);
    for (id, value, row) in matched {
        params.retain(|held| &held.0 != id);
        if let Some(value) = value.as_f64() {
            params.push((id.clone(), row.3, (value as f32).clamp(row.1, row.2)));
        }
    }
    let held: serde_json::Map<String, Value> = params.iter().map(|(id, _, value)| (id.clone(), json!(value))).collect();
    drop(params);
    drop(model);
    send_body(out, 200, "application/json", Value::Object(held).to_string().as_bytes());
}

fn post_physics(app: &App, body: Body, out: &mut TcpStream) {
    let Some(fields) = read_object(body, out, "POST /physics") else { return };
    let changes = match physics_changes(&fields) {
        Ok(changes) => changes,
        Err(line) => {
            send_body(out, 400, "text/plain", format!("{}\n", line).as_bytes());
            return;
        }
    };
    let model = app.model.lock().unwrap_or_else(PoisonError::into_inner);
    let mut physics = app.physics.lock().unwrap_or_else(PoisonError::into_inner);
    let mut values = physics.unwrap_or(model.physics);
    for (index, value) in changes {
        values[index] = value;
    }
    *physics = Some(values);
    drop(physics);
    drop(model);
    send_body(out, 200, "application/json", json!({ "strength": values[0], "wind": { "strength": values[1], "direction": values[2] }, "gravity": { "direction": values[3] } }).to_string().as_bytes());
}

fn handle_action(app: &App, out: &mut TcpStream, name: &str) {
    let unknown = format!("hikari: unknown action {}\n", name);
    if !app.model.lock().unwrap_or_else(PoisonError::into_inner).actions.iter().any(|known| known == name) {
        send_body(out, 400, "text/plain", unknown.as_bytes());
        return;
    }
    let mid_reply = app.rendering[Lane::Render as usize].load(Ordering::SeqCst) || app.rendering[Lane::RenderSplit as usize].load(Ordering::SeqCst);
    let (reply, played) = channel();
    app.jobs.send(Job::React { kind: name.to_string(), reply }).unwrap();
    let played = played.recv().unwrap();
    if !played.is_object() {
        send_body(out, 400, "text/plain", unknown.as_bytes());
        return;
    }
    send_body(out, 200, "application/json", played.to_string().as_bytes());
    println!("hikari: action {} motion={} mid_reply={}", name, played["motion"].as_str().unwrap_or("undefined"), mid_reply);
}

fn read_bg_meta(bg_dir: &Path) -> Value {
    let none = json!({ "kind": "none", "name": null, "updated": null });
    match fs::read(bg_dir.join("background.json")) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or(none),
        Err(_) => none,
    }
}

fn bg_file(bg_dir: &Path, meta: &Value) -> Option<PathBuf> {
    if meta["kind"] == "none" {
        return None;
    }
    Some(bg_dir.join(meta["file"].as_str().unwrap_or_default()))
}

fn apply_background(app: &App, bytes: Option<Arc<Vec<u8>>>, content_type: &str) -> i32 {
    let (reply, frames) = channel();
    app.jobs.send(Job::Background { bytes, content_type: content_type.to_string(), reply }).unwrap();
    frames.recv().unwrap()
}

fn clear_bg_files(bg_dir: &Path, keep: &str) {
    let Ok(entries) = fs::read_dir(bg_dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("current.") && name != keep {
            fs::remove_file(entry.path()).ok();
        }
    }
}

fn saved_background(bg_dir: &Path) -> Option<(Vec<u8>, String)> {
    let meta = read_bg_meta(bg_dir);
    let bytes = fs::read(bg_file(bg_dir, &meta)?).ok()?;
    Some((bytes, meta["type"].as_str().unwrap_or_default().to_string()))
}

fn restore_background(app: &App) {
    match saved_background(&app.bg_dir) {
        Some((bytes, content_type)) => apply_background(app, Some(Arc::new(bytes)), &content_type),
        None => apply_background(app, None, ""),
    };
}

fn handle_put_background(app: &App, out: &mut TcpStream, mut body: Body, name: &str) {
    let mut bytes = Vec::new();
    if body.by_ref().take((5 << 20) + 1).read_to_end(&mut bytes).is_err() {
        return;
    }
    if bytes.len() > 5 << 20 {
        send_body(out, 413, "", b"");
        out.shutdown(Shutdown::Both).ok();
        return;
    }
    let (content_type, ext) = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        ("image/png", "png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        ("image/jpeg", "jpg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        ("image/gif", "gif")
    } else {
        send_body(out, 400, "text/plain", b"hikari: background must be png, jpeg or gif\n");
        return;
    };
    let bytes = Arc::new(bytes);
    let _bg_guard = app.bg_lock.lock().unwrap_or_else(PoisonError::into_inner);
    let frames = apply_background(app, Some(bytes.clone()), content_type);
    if frames == 0 {
        send_body(out, 400, "", b"");
        return;
    }
    let file = format!("current.{}.{}", SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos(), ext);
    let kind = if content_type == "image/gif" { "gif" } else { "image" };
    let name = if name.is_empty() { file.clone() } else { name.to_string() };
    let meta = json!({ "kind": kind, "name": name, "updated": iso_now(), "file": file, "type": content_type, "frames": frames, "bytes": bytes.len() });
    let saved = fs::create_dir_all(&app.bg_dir).is_ok() && fs::write(app.bg_dir.join(&file), bytes.as_slice()).is_ok();
    if !saved || write_atomic(&app.bg_dir.join("background.json"), meta.to_string().as_bytes()).is_err() {
        fs::remove_file(app.bg_dir.join(&file)).ok();
        restore_background(app);
        send_body(out, 500, "text/plain", b"hikari: background save failed\n");
        return;
    }
    clear_bg_files(&app.bg_dir, &file);
    send_body(out, 200, "application/json", meta.to_string().as_bytes());
    println!("hikari: background {} {} frames={} bytes={}", kind, name, frames, bytes.len());
}

fn handle_background(app: &App, req: &Request, body: Body, out: &mut TcpStream, ready: bool) {
    if req.method == "GET" && req.path == "/background" {
        send_body(out, 200, "application/json", read_bg_meta(&app.bg_dir).to_string().as_bytes());
        return;
    }
    if req.method == "GET" {
        match saved_background(&app.bg_dir) {
            Some((bytes, content_type)) => send_body(out, 200, &content_type, &bytes),
            None => send_body(out, 404, "", b""),
        }
        return;
    }
    if !ready {
        send_body(out, 503, "", b"");
        return;
    }
    if req.method == "PUT" && req.path == "/background" {
        handle_put_background(app, out, body, &param(req, "name", ""));
        return;
    }
    if req.method == "DELETE" && req.path == "/background" {
        let _bg_guard = app.bg_lock.lock().unwrap_or_else(PoisonError::into_inner);
        apply_background(app, None, "");
        let meta = json!({ "kind": "none", "name": null, "updated": iso_now() });
        if !(fs::create_dir_all(&app.bg_dir).is_ok() && write_atomic(&app.bg_dir.join("background.json"), meta.to_string().as_bytes()).is_ok()) {
            restore_background(app);
            send_body(out, 500, "text/plain", b"hikari: background save failed\n");
            return;
        }
        clear_bg_files(&app.bg_dir, "");
        send_body(out, 200, "application/json", meta.to_string().as_bytes());
        return;
    }
    send_body(out, 400, "", b"");
}

fn handle_idle(app: &App, out: &mut TcpStream, fps: f64, size: (usize, usize), codec_id: u8) {
    let key = (size, codec_id);
    let (picture, ticked) = &app.picture;
    let mut seen = {
        let mut shown = picture.lock().unwrap_or_else(PoisonError::into_inner);
        shown.wants.push((size, codec_id, fps));
        shown.seq
    };
    out.write_all(if codec_id == MJPEG { MJPEG_HEAD } else { STREAM_HEAD }).ok();
    let t0 = Instant::now();
    let mut k = 0u64;
    let mut behind_max = 0.0f64;
    loop {
        let pts = (k as f64 * 1000.0 / fps).floor();
        wait_until(app, t0 + Duration::from_secs_f64(pts / 1000.0));
        let mut shown = picture.lock().unwrap_or_else(PoisonError::into_inner);
        while shown.seq == seen || !shown.frames.contains_key(&key) {
            shown = ticked.wait_timeout(shown, Duration::from_secs(1)).unwrap_or_else(PoisonError::into_inner).0;
        }
        seen = shown.seq;
        let (data, head) = (shown.frames[&key].clone(), shown.head);
        drop(shown);
        if !write_frame(out, pts, &head, codec_id, &data) {
            break;
        }
        behind_max = behind_max.max(t0.elapsed().as_secs_f64() * 1000.0 - pts);
        k = (k + 1).max((t0.elapsed().as_secs_f64() * fps) as u64);
    }
    let mut shown = picture.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(index) = shown.wants.iter().position(|want| want.0 == size && want.1 == codec_id && want.2 == fps) {
        shown.wants.swap_remove(index);
    }
    drop(shown);
    out.write_all(if codec_id == MJPEG { b"--frame--\r\n".as_slice() } else { b"0\r\n\r\n".as_slice() }).ok();
    println!("hikari: fps={} frames={} behind_ms_max={}", fps, k, behind_max.round());
}

fn run_picture(app: Arc<App>) {
    let mut last = Instant::now();
    let (mut start, mut ticks, mut paced) = (last, 0u32, 0.0f64);
    loop {
        let (wants, fps) = {
            let picture = app.picture.0.lock().unwrap_or_else(PoisonError::into_inner);
            let mut wants: Vec<((usize, usize), u8)> = picture.wants.iter().map(|want| (want.0, want.1)).collect();
            wants.sort();
            wants.dedup();
            (wants, picture.wants.iter().map(|want| want.2).fold(picture.window_fps, f64::max))
        };
        if fps <= 0.0 {
            thread::sleep(Duration::from_millis(20));
            last = Instant::now();
            paced = 0.0;
            continue;
        }
        ticks += 1;
        let mut deadline = start + Duration::from_secs_f64(ticks as f64 / fps);
        if fps != paced || deadline + Duration::from_secs_f64(1.0 / fps) < Instant::now() {
            (start, ticks, paced) = (last, 1, fps);
            deadline = start + Duration::from_secs_f64(1.0 / fps);
        }
        wait_until(&app, deadline);
        let now = Instant::now();
        let dt_ms = (now - last).as_secs_f32().min(0.1) * 1000.0;
        last = now;
        let mut sizes: Vec<(usize, usize)> = wants.iter().map(|want| want.0).collect();
        sizes.dedup();
        let (reply, frames) = channel();
        app.jobs.send(Job::Tick { dt_ms, sizes, reply }).unwrap();
        let (rendered, head) = frames.recv().unwrap();
        let mut encoded: HashMap<((usize, usize), u8), Arc<Vec<u8>>> = HashMap::new();
        for (size, codec_id) in wants {
            let data = match encoded.get(&(size, if codec_id == MJPEG { JPEG } else { codec_id })) {
                Some(data) => data.clone(),
                None => match rendered.iter().find(|frame| frame.0 == size) {
                    Some((_, rgba)) => Arc::new(codec::encode(rgba, size.0, size.1, CODECS[codec_id as usize])),
                    None => continue,
                },
            };
            encoded.insert((size, codec_id), data);
        }
        let mut picture = app.picture.0.lock().unwrap_or_else(PoisonError::into_inner);
        picture.seq += 1;
        picture.head = head;
        picture.frames = encoded;
        drop(picture);
        app.picture.1.notify_all();
    }
}

fn route(app: &App, req: &Request, body: Body, out: &mut TcpStream) {
    let save = match bool_param(req, "save") {
        Ok(save) => save == Some(true),
        Err(line) => {
            send_body(out, 400, "text/plain", line.as_bytes());
            return;
        }
    };
    let fps = fps_param(req);
    let codec_name = param(req, "codec", "jpeg");
    let codec_id = CODECS.iter().position(|name| *name == codec_name).map(|id| id as u8);
    let codec_ok = codec_id.is_some();
    let ready = app.ready.load(Ordering::SeqCst);
    let invalid = if ready { 400 } else { 503 };
    let streams = matches!((req.method.as_str(), req.path.as_str()), ("POST", "/talk") | ("GET", "/listen"));
    let size = match size_param(req, app.size) {
        Ok(_) if codec_name == "split" => SPLIT_SIZE,
        Ok(size) => size,
        Err(line) if streams => {
            send_body(out, 400, "text/plain", line.as_bytes());
            return;
        }
        Err(_) => app.size,
    };
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/models") => {
            if !ready {
                send_body(out, 503, "", b"");
                return;
            }
            let broken = app.broken.lock().unwrap_or_else(PoisonError::into_inner);
            let names: Vec<&String> = app.models.keys().filter(|name| !broken.contains(name)).collect();
            send_body(out, 200, "application/json", json!({ "model": app.model.lock().unwrap_or_else(PoisonError::into_inner).name.clone(), "models": names }).to_string().as_bytes());
        }
        ("POST", "/talk") => {
            let fps = match fps {
                Ok(fps) => fps,
                Err(line) => {
                    send_body(out, 400, "text/plain", line.as_bytes());
                    return;
                }
            };
            if !ready || !codec_ok {
                send_body(out, invalid, "", b"");
                return;
            }
            let mut body = body;
            match read_wav_header(&mut body) {
                Ok((rate, channels, limit)) => handle_render(app, out, body.take(limit), fps, (rate, channels), size, &codec_name, codec_id.unwrap()),
                Err(line) => {
                    println!("{}", line);
                    send_body(out, 400, "text/plain", format!("{}\n", line).as_bytes());
                }
            }
        }
        ("POST", "/state") => post_state(app, req, out),
        ("GET", "/state") => {
            let states = app.model.lock().unwrap_or_else(PoisonError::into_inner).states.clone();
            let state = app.state.lock().unwrap_or_else(PoisonError::into_inner).clone();
            send_body(out, 200, "application/json", json!({ "face": state.face, "pose": state.pose, "asleep": app.asleep.load(Ordering::SeqCst), "head_lead_ms": app.head_lead_ms.load(Ordering::SeqCst), "states": states }).to_string().as_bytes());
        }
        ("POST", "/action") => {
            if !ready {
                send_body(out, 503, "", b"");
                return;
            }
            handle_action(app, out, &param(req, "action", ""));
        }
        ("GET", "/action") => {
            let actions = app.model.lock().unwrap_or_else(PoisonError::into_inner).actions.clone();
            let idle = app.idle.lock().unwrap_or_else(PoisonError::into_inner).actions.clone();
            send_body(out, 200, "application/json", json!({ "actions": actions, "idle": idle }).to_string().as_bytes());
        }
        ("POST", "/params") | ("POST", "/physics") => {
            if !ready {
                send_body(out, 503, "", b"");
                return;
            }
            if req.path == "/params" {
                post_params(app, body, out);
            } else {
                post_physics(app, body, out);
            }
        }
        ("GET", "/settings") => send_body(out, 200, "application/json", settings_json(app).to_string().as_bytes()),
        ("PUT", "/settings") => put_settings(app, body, out),
        (_, "/background") | (_, "/background/file") => handle_background(app, req, body, out, ready),
        ("GET", "/listen") => {
            let fps = match fps {
                Ok(fps) => fps,
                Err(line) => {
                    send_body(out, 400, "text/plain", line.as_bytes());
                    return;
                }
            };
            if !ready || !codec_ok {
                send_body(out, invalid, "", b"");
                return;
            }
            handle_idle(app, out, fps, size, codec_id.unwrap());
        }
        ("PUT", "/view") => {
            if !ready {
                send_body(out, 503, "", b"");
                return;
            }
            put_view(app, req, out, save);
        }
        ("GET", "/view") => {
            let Model { name, framing, .. } = app.model.lock().unwrap_or_else(PoisonError::into_inner).clone();
            let views = app.views.lock().unwrap_or_else(PoisonError::into_inner);
            let view = model_view(&views, &name, framing);
            let saved = views.saved.get(&name).map(|v| json!({ "zoom": v[0], "x": v[1], "y": v[2], "rot": v[3] }));
            let default = resolve_view(MODEL_FRAMING, framing);
            send_body(out, 200, "application/json", json!({ "zoom": view[0], "x": view[1], "y": view[2], "rot": view[3], "saved": saved, "default": { "zoom": default[0], "x": default[1], "y": default[2], "rot": default[3] } }).to_string().as_bytes());
        }
        _ => send_body(out, 404, "", b""),
    }
}

fn wait_until(app: &App, deadline: Instant) {
    let mut event = KEvent { ident: 0, filter: 0, flags: 0, fflags: 0, data: 0, udata: std::ptr::null_mut() };
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let timeout = TimeSpec { tv_sec: left.as_secs() as i64, tv_nsec: left.subsec_nanos() as i64 };
        unsafe { kevent(app.timer_queue, std::ptr::null(), 0, &mut event, 1, &timeout) };
    }
}

fn parse_request(reader: &mut BufReader<TcpStream>) -> Option<Request> {
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?;
    let (path, query_text) = target.split_once('?').unwrap_or((target, ""));
    let mut query = Vec::new();
    for pair in query_text.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let mut decoded = Vec::new();
        for text in [key, value] {
            let bytes = text.as_bytes();
            let mut out = Vec::with_capacity(bytes.len());
            let mut i = 0;
            while i < bytes.len() {
                if bytes[i] == b'+' {
                    out.push(b' ');
                    i += 1;
                    continue;
                }
                if bytes[i] == b'%' && i + 3 <= bytes.len() {
                    if let (Some(high), Some(low)) = ((bytes[i + 1] as char).to_digit(16), (bytes[i + 2] as char).to_digit(16)) {
                        out.push((high * 16 + low) as u8);
                        i += 3;
                        continue;
                    }
                }
                out.push(bytes[i]);
                i += 1;
            }
            decoded.push(String::from_utf8_lossy(&out).into_owned());
        }
        let value = decoded.pop().unwrap();
        let key = decoded.pop().unwrap();
        query.push((key, value));
    }
    let mut headers = Vec::new();
    loop {
        let mut header_line = String::new();
        if reader.read_line(&mut header_line).ok()? == 0 {
            return None;
        }
        if header_line.trim().is_empty() {
            break;
        }
        if let Some((key, value)) = header_line.split_once(':') {
            headers.push((key.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    Some(Request { method, path: path.to_string(), query, headers })
}

fn handle_connection(app: Arc<App>, mut out: TcpStream) {
    out.set_nodelay(true).ok();
    let Ok(read_half) = out.try_clone() else { return };
    let Ok(mut drain) = out.try_clone() else { return };
    let mut reader = BufReader::new(read_half);
    out.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let Some(req) = parse_request(&mut reader) else { return };
    out.set_read_timeout(None).ok();
    if header(&req, "expect").eq_ignore_ascii_case("100-continue") {
        out.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").ok();
    }
    let chunked = header(&req, "transfer-encoding").to_ascii_lowercase().contains("chunked");
    let remaining = if chunked { 0 } else { header(&req, "content-length").parse().unwrap_or(0) };
    let body = Body { reader, remaining, chunked, done: false };
    route(&app, &req, body, &mut out);
    out.shutdown(Shutdown::Write).ok();
    drain.set_read_timeout(Some(Duration::from_secs(2))).ok();
    io::copy(&mut drain, &mut io::sink()).ok();
}

fn load_background(engines: &[*mut engine_ffi::Engine], bytes: Option<&[u8]>, content_type: &str) -> i32 {
    let Some(bytes) = bytes else {
        for engine in engines {
            unsafe { engine_ffi::engine_background_clear(*engine) };
        }
        return 1;
    };
    let Ok(expected) = CString::new(content_type) else { return 0 };
    let Some(first) = engines.first() else { return 0 };
    let frames = unsafe { engine_ffi::engine_background_load(*first, bytes.as_ptr(), bytes.len(), expected.as_ptr()) };
    if frames > 0 {
        for engine in &engines[1..] {
            unsafe { engine_ffi::engine_background_load(*engine, bytes.as_ptr(), bytes.len(), expected.as_ptr()) };
        }
    }
    frames
}

fn sync_state(app: &App, lane: &mut LaneState) {
    let current = app.state.lock().unwrap_or_else(PoisonError::into_inner);
    if lane.state_generation == current.generation {
        return;
    }
    let state = current.clone();
    drop(current);
    lane.state_generation = state.generation;
    lane.behavior.set_state(1, &state.pose);
    lane.behavior.set_state(0, &state.face);
}

fn resolve_view(view: [f32; 4], framing: Option<[f32; 4]>) -> [f32; 4] {
    let base = framing.unwrap_or([1.0, 0.0, 0.0, 0.0]);
    std::array::from_fn(|i| if view[i].is_finite() { view[i] } else { base[i] })
}

fn model_view(views: &Views, name: &str, framing: Option<[f32; 4]>) -> [f32; 4] {
    views.live.get(name).or(views.saved.get(name)).copied().unwrap_or_else(|| resolve_view(MODEL_FRAMING, framing))
}

fn load_views(path: &Path) -> HashMap<String, [f32; 4]> {
    let mut saved = HashMap::new();
    let Ok(text) = fs::read_to_string(path) else { return saved };
    let root: Value = match serde_json::from_str(&text) {
        Ok(root) => root,
        Err(error) => {
            eprintln!("hikari: views.json {}", error);
            return saved;
        }
    };
    let Some(rows) = root.as_object() else {
        eprintln!("hikari: views.json must be an object of model names, ignoring it");
        return saved;
    };
    let mut invalid = Vec::new();
    for (name, row) in rows {
        let view = ["zoom", "x", "y", "rot"].map(|key| row[key].as_f64().unwrap_or(f64::NAN) as f32);
        if view.iter().all(|v| v.is_finite()) && view[0] > 0.0 {
            saved.insert(name.clone(), view);
        } else {
            invalid.push(name.as_str());
        }
    }
    if !invalid.is_empty() {
        eprintln!("hikari: views.json skipped invalid views for {}", invalid.join(", "));
    }
    saved
}

fn save_view(views: &mut Views, changes: &[(String, [f32; 4])]) -> Result<(), String> {
    let mut saved = views.saved.clone();
    saved.extend(changes.iter().cloned());
    let rows: serde_json::Map<String, Value> = saved.iter().map(|(name, v)| (name.clone(), json!({ "zoom": v[0], "x": v[1], "y": v[2], "rot": v[3] }))).collect();
    write_atomic(&views.path, Value::Object(rows).to_string().as_bytes())?;
    views.saved = saved;
    for (name, _) in changes {
        println!("hikari: saved view for {}", name);
    }
    Ok(())
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    static SAVES: AtomicU64 = AtomicU64::new(0);
    let mut temp = path.to_path_buf();
    let mut file = None;
    for _ in 0..100 {
        let save = SAVES.fetch_add(1, Ordering::Relaxed) + 1;
        temp = path.with_extension(format!("json.{}.{}.tmp", std::process::id(), save));
        match fs::OpenOptions::new().write(true).create_new(true).open(&temp) {
            Ok(created) => {
                file = Some(created);
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(_) => break,
        }
    }
    let Some(mut file) = file else {
        let line = format!("hikari: can't write {} (no temp file)", path.display());
        eprintln!("{}", line);
        return Err(line);
    };
    if let Err(error) = file.write_all(bytes).and_then(|_| file.sync_all()).and_then(|_| fs::rename(&temp, path)) {
        fs::remove_file(&temp).ok();
        let line = format!("hikari: can't write {} ({})", path.display(), error);
        eprintln!("{}", line);
        return Err(line);
    }
    Ok(())
}

fn sync_idle(app: &App, state: &mut LaneState) {
    let idle = app.idle.lock().unwrap_or_else(PoisonError::into_inner);
    if state.idle_generation != idle.generation {
        if let Ok(every) = parse_every(&idle.every) {
            state.behavior.set_actions(&idle.actions, every);
        }
        state.idle_generation = idle.generation;
    }
}

fn parse_every(value: &Value) -> Result<(u64, u64), String> {
    let items = match value {
        Value::Array(items) => items.clone(),
        Value::Number(_) => vec![value.clone(), value.clone()],
        _ => Vec::new(),
    };
    let seconds: Vec<u64> = items.iter().filter_map(Value::as_f64).filter(|seconds| *seconds > 0.0 && seconds.fract() == 0.0).map(|seconds| seconds as u64).collect();
    match (items.len(), seconds.as_slice()) {
        (2, &[min, max]) if min <= max => Ok((min, max)),
        _ => Err(format!("hikari: action_every must be whole seconds above 0 or [min, max], got {}", value)),
    }
}

fn parse_idle(idle_actions: Option<&Value>, action_every: Option<&Value>, actions: &[String], every: &Value, valid: &[String]) -> Result<(Vec<String>, Value, Vec<String>, Vec<String>), String> {
    let names: Vec<String> = match idle_actions {
        None => actions.to_vec(),
        Some(Value::Array(items)) if items.iter().all(Value::is_string) => items.iter().filter_map(Value::as_str).map(str::to_string).collect(),
        Some(other) => return Err(format!("hikari: idle_actions in settings.json must be a list of action names, got {}", other)),
    };
    let every = action_every.unwrap_or(every).clone();
    parse_every(&every)?;
    let (banned, names): (Vec<String>, Vec<String>) = names.into_iter().partition(|name| name == "sleep");
    let (known, unknown) = names.into_iter().partition(|name| valid.contains(name));
    Ok((known, every, unknown, banned))
}

fn read_transition_seconds(value: Option<&Value>) -> Result<Option<f64>, &Value> {
    let Some(value) = value else { return Ok(None) };
    match value.as_f64() {
        Some(seconds) if seconds.is_finite() && (0.0..=TRANSITION_SECONDS_MAX).contains(&seconds) => Ok(Some(seconds)),
        _ => Err(value),
    }
}

fn settings_json(app: &App) -> Value {
    let idle = app.idle.lock().unwrap_or_else(PoisonError::into_inner);
    json!({ "model": app.model.lock().unwrap_or_else(PoisonError::into_inner).name.clone(), "size": [app.size.0, app.size.1], "idle_actions": idle.actions, "action_every": idle.every, "face": idle.face, "idle_loop": idle.idle_loop, "transition_seconds": f64::from_bits(app.transition_seconds.load(Ordering::SeqCst)), "physics": app.settings_physics.lock().unwrap_or_else(PoisonError::into_inner).clone() })
}

fn put_settings(app: &App, body: Body, out: &mut TcpStream) {
    let Some(fields) = read_object(body, out, "PUT /settings") else { return };
    let _settings_guard = app.settings_lock.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(key) = fields.keys().find(|key| ["size", "port", "idle_loop"].contains(&key.as_str())) {
        send_body(out, 400, "text/plain", format!("hikari: {} is restart-only, edit settings.json and restart\n", key).as_bytes());
        return;
    }
    if let Some(key) = fields.keys().find(|key| !["idle_actions", "action_every", "model", "face", "transition_seconds", "physics"].contains(&key.as_str())) {
        send_body(out, 400, "text/plain", format!("hikari: unknown setting {}\n", key).as_bytes());
        return;
    }
    let transition_seconds = match read_transition_seconds(fields.get("transition_seconds")) {
        Ok(seconds) => seconds,
        Err(other) => {
            send_body(out, 400, "text/plain", format!("hikari: transition_seconds must be a number 0..{}, got {}\n", TRANSITION_SECONDS_MAX, other).as_bytes());
            return;
        }
    };
    let physics = match fields.get("physics") {
        None => None,
        Some(Value::Object(physics)) => match physics_changes(physics) {
            Ok(changes) => Some((physics.clone(), changes)),
            Err(line) => {
                send_body(out, 400, "text/plain", format!("{}\n", line).as_bytes());
                return;
            }
        },
        Some(_) => {
            send_body(out, 400, "text/plain", b"hikari: physics must be an object\n");
            return;
        }
    };
    let no_reload = (physics.is_some() || transition_seconds.is_some()) && fields.len() == physics.iter().count() + transition_seconds.iter().count();
    let model = match fields.get("model") {
        None => None,
        Some(Value::String(name)) if app.models.contains_key(name) => Some(name.clone()),
        Some(other) => {
            send_body(out, 400, "text/plain", format!("hikari: unknown model {}\n", other).as_bytes());
            return;
        }
    };
    let (path, actions, every) = {
        let idle = app.idle.lock().unwrap_or_else(PoisonError::into_inner);
        (idle.path.clone(), idle.actions.clone(), idle.every.clone())
    };
    let current = app.model.lock().unwrap_or_else(PoisonError::into_inner).clone();
    let setup = match &model {
        Some(name) if *name != current.name => Some(vtube::load(&app.models[name])),
        _ => None,
    };
    let target = match (&setup, &model) {
        (Some(setup), Some(name)) => describe_model(name, setup, &app.shipped, &app.default_actions, &app.settings_physics.lock().unwrap_or_else(PoisonError::into_inner)),
        _ => current,
    };
    let face = match fields.get("face") {
        None => None,
        Some(Value::String(name)) if target.states.contains(name) => Some(name.clone()),
        Some(other) => {
            send_body(out, 400, "text/plain", format!("hikari: unknown state {}\n", other.as_str().map_or(other.to_string(), str::to_string)).as_bytes());
            return;
        }
    };
    let (actions, every) = match parse_idle(fields.get("idle_actions"), fields.get("action_every"), &actions, &every, &target.actions) {
        Ok((_, _, _, banned)) if !banned.is_empty() => {
            send_body(out, 400, "text/plain", format!("hikari: {} can't be an idle action\n", banned[0]).as_bytes());
            return;
        }
        Ok((_, _, unknown, _)) if !unknown.is_empty() => {
            send_body(out, 400, "text/plain", format!("hikari: unknown idle actions {}\n", unknown.join(", ")).as_bytes());
            return;
        }
        Ok((actions, every, _, _)) => (actions, every),
        Err(line) => {
            send_body(out, 400, "text/plain", format!("{}\n", line).as_bytes());
            return;
        }
    };
    let mut file = match fs::read_to_string(&path) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(fields)) => fields,
            _ => {
                send_body(out, 500, "text/plain", format!("hikari: {} is not a JSON object, not rewriting it\n", path.display()).as_bytes());
                return;
            }
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => serde_json::Map::new(),
        Err(error) => {
            send_body(out, 500, "text/plain", format!("hikari: can't read {} ({}), not rewriting it\n", path.display(), error).as_bytes());
            return;
        }
    };
    file.remove("default_model");
    file.extend(fields);
    let mut pretty = serde_json::to_string_pretty(&Value::Object(file)).unwrap_or_default();
    pretty.push('\n');
    let switch_to = model.filter(|name| *name != app.model.lock().unwrap_or_else(PoisonError::into_inner).name);
    let _bg_guard = switch_to.as_ref().map(|_| app.bg_lock.lock().unwrap_or_else(PoisonError::into_inner));
    let mut scene = None;
    if let Some(name) = &switch_to {
        app.shared.lock().unwrap_or_else(PoisonError::into_inner).pending.push(name.clone());
        scene = build_scene(app, name, setup);
        if scene.is_none() {
            unpin(app, name);
            send_body(out, 500, "text/plain", format!("hikari: model {} failed to load\n", name).as_bytes());
            return;
        }
    }
    if let Err(line) = write_atomic(&path, pretty.as_bytes()) {
        if let Some(scene) = scene {
            drop_scene(app, scene);
        }
        if let Some(name) = &switch_to {
            unpin(app, name);
        }
        send_body(out, 500, "text/plain", format!("{}\n", line).as_bytes());
        return;
    }
    if let (Some(scene), Some(name)) = (scene, &switch_to) {
        let (reply, old) = channel();
        app.jobs.send(Job::Switch { scene, reply }).unwrap();
        if let Ok(old) = old.recv() {
            drop_scene(app, old);
        }
        unpin(app, name);
    }
    let mut idle = app.idle.lock().unwrap_or_else(PoisonError::into_inner);
    println!("hikari: idle actions={} action_every={}", actions.join(","), every);
    idle.actions = actions;
    idle.every = every;
    if !no_reload {
        idle.generation += 1;
    }
    if let Some(seconds) = transition_seconds {
        app.transition_seconds.store(seconds.to_bits(), Ordering::SeqCst);
    }
    if let Some(face) = face {
        idle.face = face.clone();
        let mut state = app.state.lock().unwrap_or_else(PoisonError::into_inner);
        if !state.face_given && state.face != face {
            state.face = face;
            state.generation += 1;
        }
    }
    drop(idle);
    if let Some((physics, changes)) = physics {
        let mut model = app.model.lock().unwrap_or_else(PoisonError::into_inner);
        model.physics = effective_physics(model.own_physics, &changes);
        *app.physics.lock().unwrap_or_else(PoisonError::into_inner) = Some(model.physics);
        *app.settings_physics.lock().unwrap_or_else(PoisonError::into_inner) = physics;
    }
    send_body(out, 200, "application/json", settings_json(app).to_string().as_bytes());
}

fn build_lanes(app: &App, name: &str, sizes: &[(usize, usize)], setup: Option<vtube::VtubeSetup>) -> Option<(Vec<LaneState>, vtube::VtubeSetup)> {
    let path = app.models.get(name)?.to_string_lossy().into_owned();
    let model = CString::new(path.as_str()).ok()?;
    let setup = match setup {
        Some(setup) => setup,
        None => vtube::load(Path::new(&path)),
    };
    let physics = setup.physics.map_or("none".to_string(), |(strength, wind)| format!("{},{}", strength, wind));
    println!("hikari: vtube model={} params={} expressions={} physics={}", name, setup.outputs.values().map(Vec::len).sum::<usize>(), setup.expressions.len(), physics);
    let view = model_view(&app.views.lock().unwrap_or_else(PoisonError::into_inner), name, setup.framing);
    println!("hikari: view model={} zoom={} x={} y={} rot={}", name, view[0], view[1], view[2], view[3]);
    let physics = effective_physics(setup.physics, &physics_changes(&app.settings_physics.lock().unwrap_or_else(PoisonError::into_inner)).unwrap_or_default());
    let mut lanes: Vec<LaneState> = Vec::new();
    for &(width, height) in sizes {
        let engine = unsafe { engine_ffi::engine_create(model.as_ptr(), width as i32, height as i32) };
        if engine.is_null() {
            eprintln!("hikari: engine_create failed for {}", path);
            for state in &lanes {
                unsafe { engine_ffi::engine_destroy(state.engine) };
            }
            unsafe { engine_ffi::engine_textures_release(model.as_ptr()) };
            return None;
        }
        unsafe { engine_ffi::engine_physics_set(engine, physics[0], physics[1], physics[2], physics[3]) };
        for (param, strength, wind) in &setup.physics_scales {
            if let Ok(id) = CString::new(param.as_str()) {
                unsafe { engine_ffi::engine_physics_scale(engine, id.as_ptr(), *strength, *wind) };
            }
        }
        for active in &setup.active_expressions {
            if let Ok(id) = CString::new(active.as_str()) {
                unsafe { engine_ffi::engine_expression_keep(engine, id.as_ptr()) };
            }
        }
        let idle_loop = app.idle.lock().unwrap_or_else(PoisonError::into_inner).idle_loop.clone();
        let mut behavior = Behavior::new(engine, &setup, &app.shipped, &app.default_actions, &idle_loop);
        unsafe { engine_ffi::engine_view_set(engine, view[0], view[1], view[2], view[3]) };
        behavior.update(0.0, -1.0);
        unsafe { engine_ffi::engine_render(engine, behavior.bg_clock_ms as f32) };
        let gpu = unsafe { CStr::from_ptr(engine_ffi::engine_gpu_name(engine)) }.to_string_lossy().into_owned();
        println!("hikari: page ready gpu={}", gpu);
        lanes.push(LaneState { width, height, engine, behavior, framing: setup.framing, view, physics, idle_generation: 0, state_generation: 0 });
    }
    for (id, output) in &lanes.first()?.behavior.remap {
        println!("hikari: vtube remap model={} {}->{}", name, id.to_string_lossy(), output.to_string_lossy());
    }
    if let Some((bytes, content_type)) = saved_background(&app.bg_dir) {
        load_background(&lanes.iter().map(|state| state.engine).collect::<Vec<_>>(), Some(&bytes), &content_type);
    }
    Some((lanes, setup))
}

fn build_scene(app: &App, name: &str, setup: Option<vtube::VtubeSetup>) -> Option<Scene> {
    let started = Instant::now();
    let Some((lanes, setup)) = build_lanes(app, name, &[app.size], setup) else {
        eprintln!("hikari: model {} failed to load", name);
        app.broken.lock().unwrap_or_else(PoisonError::into_inner).push(name.to_string());
        return None;
    };
    println!("hikari: model {} ready ms={}", name, started.elapsed().as_millis());
    let mut model = describe_model(name, &setup, &app.shipped, &app.default_actions, &app.settings_physics.lock().unwrap_or_else(PoisonError::into_inner));
    let (mut minimum, mut maximum) = (0.0f32, 0.0f32);
    let mut index = 0;
    loop {
        let id = unsafe { engine_ffi::engine_param_at(lanes[0].engine, index, &mut minimum, &mut maximum) };
        if id.is_null() {
            break;
        }
        model.params.push((unsafe { CStr::from_ptr(id) }.to_string_lossy().into_owned(), minimum, maximum, index));
        index += 1;
    }
    for (id, output) in &lanes[0].behavior.remap {
        if let Some(row) = model.params.iter().find(|row| row.0.as_bytes() == output.as_bytes()).cloned() {
            model.params.push((id.to_string_lossy().into_owned(), row.1, row.2, row.3));
        }
    }
    Some(Scene { model, lanes, targets: HashMap::new(), asleep: false, stepped_since_tick: false })
}

fn drop_scene(app: &App, scene: Scene) {
    for state in &scene.lanes {
        unsafe { engine_ffi::engine_destroy(state.engine) };
    }
    for (engine, _) in scene.targets.values() {
        unsafe { engine_ffi::engine_destroy(*engine) };
    }
    release_textures(app, &scene.model.name);
    println!("hikari: model {} dropped", scene.model.name);
}

fn unpin(app: &App, name: &str) {
    let mut shared = app.shared.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(index) = shared.pending.iter().position(|pinned| pinned == name) {
        shared.pending.remove(index);
    }
}

fn release_textures(app: &App, name: &str) {
    let shared = app.shared.lock().unwrap_or_else(PoisonError::into_inner);
    if shared.name == name || shared.window == name || shared.pending.iter().any(|pinned| pinned == name) {
        return;
    }
    if let Ok(path) = CString::new(app.models[name].to_string_lossy().as_bytes()) {
        unsafe { engine_ffi::engine_textures_release(path.as_ptr()) };
    }
}

impl Scene {
    fn render(&mut self, app: &App, lane: Lane, size: (usize, usize)) -> Vec<u8> {
        let state = &self.lanes[lane as usize];
        let engine = if (state.width, state.height) == size {
            state.engine
        } else {
            let target = self.targets.entry(size).or_insert_with(|| (std::ptr::null_mut(), Instant::now()));
            if target.0.is_null() {
                let path = CString::new(app.models[&self.model.name].to_string_lossy().as_bytes()).unwrap_or_default();
                target.0 = unsafe { engine_ffi::engine_create(path.as_ptr(), size.0 as i32, size.1 as i32) };
                if target.0.is_null() {
                    eprintln!("hikari: engine_create failed for {}x{}", size.0, size.1);
                    self.targets.remove(&size);
                    return vec![0; size.0 * size.1 * 4];
                }
                if let Some((bytes, content_type)) = saved_background(&app.bg_dir) {
                    load_background(&[target.0], Some(&bytes), &content_type);
                }
                println!("hikari: target {}x{} model={}", size.0, size.1, self.model.name);
            }
            target.1 = Instant::now();
            target.0
        };
        let pixels = unsafe {
            engine_ffi::engine_view_set(engine, state.view[0], state.view[1], state.view[2], state.view[3]);
            if engine != state.engine {
                engine_ffi::engine_mirror(engine, state.engine);
            }
            engine_ffi::engine_render(engine, state.behavior.bg_clock_ms as f32)
        };
        unsafe { std::slice::from_raw_parts(pixels, size.0 * size.1 * 4) }.to_vec()
    }

    fn prepare(&mut self, app: &App, lane: Lane) {
        let state = &mut self.lanes[lane as usize];
        unsafe { engine_ffi::engine_transition_seconds(state.engine, f64::from_bits(app.transition_seconds.load(Ordering::SeqCst)) as f32) };
        sync_idle(app, state);
        sync_state(app, state);
        state.behavior.head_lead_ms = app.head_lead_ms.load(Ordering::SeqCst) as f32;
        state.behavior.params.clear();
        state.behavior.params.extend(app.params.lock().unwrap_or_else(PoisonError::into_inner).iter().map(|row| (row.1, row.2)));
        let physics = app.physics.lock().unwrap_or_else(PoisonError::into_inner).unwrap_or(self.model.physics);
        if physics != state.physics {
            unsafe { engine_ffi::engine_physics_set(state.engine, physics[0], physics[1], physics[2], physics[3]) };
            state.physics = physics;
        }
        state.view = model_view(&app.views.lock().unwrap_or_else(PoisonError::into_inner), &self.model.name, state.framing);
    }

    fn sleep(&mut self, app: &App) -> Value {
        if self.asleep {
            return json!({ "action": "sleep", "motion": "sleep", "expression": null });
        }
        let played = self.lanes[Lane::Shared as usize].behavior.sleep_begin();
        if played.is_object() {
            self.asleep = true;
            app.asleep.store(true, Ordering::SeqCst);
            println!("hikari: asleep");
        }
        played
    }

    fn wake(&mut self, app: &App) {
        if !std::mem::take(&mut self.asleep) {
            return;
        }
        let shared = &mut self.lanes[Lane::Shared as usize];
        shared.behavior.sleep_end();
        unsafe { engine_ffi::engine_carry(shared.engine, shared.engine, WAKE_MS) };
        app.asleep.store(false, Ordering::SeqCst);
        println!("hikari: wake");
    }
}

fn run_render(app: Arc<App>, jobs: Receiver<Job>, port: u16) {
    let wanted = app.model.lock().unwrap_or_else(PoisonError::into_inner).name.clone();
    let order = std::iter::once(wanted.clone()).chain(app.models.keys().filter(|name| **name != wanted).cloned());
    let Some(mut scene) = order.into_iter().find_map(|name| build_scene(&app, &name, None)) else {
        eprintln!("hikari: no model loads");
        std::process::exit(1);
    };
    *app.model.lock().unwrap_or_else(PoisonError::into_inner) = scene.model.clone();
    let mut shared = app.shared.lock().unwrap_or_else(PoisonError::into_inner);
    shared.engine = scene.lanes[Lane::Shared as usize].engine as usize;
    shared.name = scene.model.name.clone();
    drop(shared);
    app.ready.store(true, Ordering::SeqCst);
    let core = unsafe { engine_ffi::csmGetVersion() };
    println!("hikari: listening 127.0.0.1:{} models={} model={} core={}.{}.{}", port, app.models.len(), scene.model.name, core >> 24, (core >> 16) & 0xff, core & 0xffff);
    for job in jobs {
        match job {
            Job::Tick { dt_ms, sizes, reply } => {
                let now = Instant::now();
                let mouth = app.mouths.lock().unwrap_or_else(PoisonError::into_inner).mouth_now(now);
                scene.prepare(&app, Lane::Shared);
                let mut shared = app.shared.lock().unwrap_or_else(PoisonError::into_inner);
                let state = &mut scene.lanes[Lane::Shared as usize];
                if !std::mem::take(&mut scene.stepped_since_tick) {
                    state.behavior.update(dt_ms, mouth);
                }
                shared.bg_clock_ms = state.behavior.bg_clock_ms;
                drop(shared);
                let head = scene.lanes[Lane::Shared as usize].behavior.head;
                let frames = sizes.into_iter().map(|size| (size, scene.render(&app, Lane::Shared, size))).collect();
                scene.targets.retain(|_, target| {
                    let keep = target.1.elapsed() < TARGET_IDLE;
                    if !keep {
                        unsafe { engine_ffi::engine_destroy(target.0) };
                    }
                    keep
                });
                reply.send((frames, head)).ok();
            }
            Job::Step { owner, dt_ms, mouth, size, reply } => {
                scene.prepare(&app, Lane::Shared);
                let mut mouths = app.mouths.lock().unwrap_or_else(PoisonError::into_inner);
                if mouths.owner == 0 {
                    mouths.owner = owner;
                    mouths.fps = 0.0;
                }
                let advance = mouths.owner == owner;
                drop(mouths);
                if advance {
                    scene.lanes[Lane::Shared as usize].behavior.update(dt_ms, mouth);
                    scene.stepped_since_tick = true;
                }
                let rgba = scene.render(&app, Lane::Shared, size);
                reply.send((rgba, scene.lanes[Lane::Shared as usize].behavior.head)).ok();
            }
            Job::React { kind, reply } => {
                if kind == "sleep" {
                    reply.send(scene.sleep(&app)).ok();
                    continue;
                }
                scene.wake(&app);
                let played = scene.lanes[Lane::Shared as usize].behavior.react(&kind);
                reply.send(played).ok();
            }
            Job::Wake => scene.wake(&app),
            Job::Background { bytes, content_type, reply } => {
                let engines: Vec<*mut engine_ffi::Engine> = scene.lanes.iter().map(|state| state.engine).chain(scene.targets.values().map(|target| target.0)).collect();
                let frames = load_background(&engines, bytes.as_deref().map(Vec::as_slice), &content_type);
                if frames > 0 {
                    *app.window_background.lock().unwrap_or_else(PoisonError::into_inner) = Some((bytes, content_type));
                }
                reply.send(frames).ok();
            }
            Job::Switch { scene: next, reply } => {
                let model = next.model.name.clone();
                let mut shared = app.shared.lock().unwrap_or_else(PoisonError::into_inner);
                let old = std::mem::replace(&mut scene, next);
                shared.engine = scene.lanes[Lane::Shared as usize].engine as usize;
                shared.name = model.clone();
                shared.bg_clock_ms = 0.0;
                let mut current = app.model.lock().unwrap_or_else(PoisonError::into_inner);
                *current = scene.model.clone();
                app.params.lock().unwrap_or_else(PoisonError::into_inner).clear();
                *app.physics.lock().unwrap_or_else(PoisonError::into_inner) = None;
                drop(current);
                app.views.lock().unwrap_or_else(PoisonError::into_inner).live.remove(&model);
                let face = app.idle.lock().unwrap_or_else(PoisonError::into_inner).face.clone();
                let mut state = app.state.lock().unwrap_or_else(PoisonError::into_inner);
                *state = ModelState { face, pose: "none".to_string(), face_given: false, generation: state.generation + 1 };
                drop(state);
                app.asleep.store(false, Ordering::SeqCst);
                drop(shared);
                println!("hikari: model switched to {}", model);
                reply.send(old).ok();
            }
        }
    }
}

fn discover_models(exe_dir: &Path) -> BTreeMap<String, PathBuf> {
    let mut models = BTreeMap::new();
    for root in [exe_dir.join("models"), PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../models"))] {
        let Ok(dirs) = fs::read_dir(&root) else { continue };
        for dir in dirs.flatten() {
            let Ok(files) = fs::read_dir(dir.path()) else { continue };
            for file in files.flatten() {
                let file_name = file.file_name().to_string_lossy().into_owned();
                if file_name.starts_with('.') {
                    continue;
                }
                if let Some(name) = file_name.strip_suffix(".model3.json") {
                    models.entry(name.to_string()).or_insert(file.path());
                }
            }
        }
    }
    models
}

fn read_settings(exe_dir: &Path) -> (Value, PathBuf) {
    let paths = [exe_dir.join("settings.json"), PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../settings.json"))];
    let Some((text, path)) = paths.iter().find_map(|path| fs::read_to_string(path).ok().map(|text| (text, path.clone()))) else { return (Value::Null, paths[0].clone()) };
    match serde_json::from_str(&text) {
        Ok(Value::Object(settings)) => (Value::Object(settings), path),
        Ok(_) => {
            eprintln!("hikari: {} must be a JSON object", path.display());
            std::process::exit(1);
        }
        Err(error) => {
            eprintln!("hikari: {} is not valid JSON ({})", path.display(), error);
            std::process::exit(1);
        }
    }
}

fn setting_size(settings: &Value, key: &str, default: (usize, usize)) -> (usize, usize) {
    let Some(value) = settings.get(key) else { return default };
    let parts = match value.as_array().map(Vec::as_slice) {
        Some([width, height]) => (width.as_u64(), height.as_u64()),
        _ => (None, None),
    };
    match parts {
        (Some(width), Some(height)) if (1..=3840).contains(&width) && (1..=2160).contains(&height) => (width as usize, height as usize),
        _ => {
            eprintln!("hikari: {} in settings.json must be [width, height] with positive integers up to [3840, 2160], got {}", key, value);
            std::process::exit(1);
        }
    }
}

fn setting_model(settings: &Value, models: &BTreeMap<String, PathBuf>) -> String {
    let first = models.keys().next().cloned().unwrap_or_default();
    match &settings["model"] {
        Value::Null => first,
        Value::String(name) if name.is_empty() => first,
        Value::String(name) if models.contains_key(name) => name.to_string(),
        other => {
            eprintln!("hikari: model {} not found, using {}", other, first);
            first
        }
    }
}

fn main() {
    unsafe { engine_ffi::engine_core_load() };
    let args: Vec<String> = std::env::args().collect();
    let exe = std::env::current_exe().unwrap();
    let exe_dir = exe.parent().unwrap().to_path_buf();
    let (settings, settings_path) = read_settings(&exe_dir);
    let mut port_arg = None;
    let mut window = false;
    let mut fps = 60u32;
    let mut share = true;
    let mut settings_only = false;
    let mut no_settings = false;
    let mut rest = args[1..].iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--settings" => settings_only = true,
            "--no-settings" => no_settings = true,
            "--port" => {
                let value = rest.next().cloned().unwrap_or_default();
                port_arg = port_arg.or(Some(value));
            }
            "--window" if !window => window = true,
            _ if !window => {}
            "--fps" => match rest.next().and_then(|value| value.parse().ok()) {
                Some(value @ 1..=240) => fps = value,
                _ => {
                    eprintln!("hikari: --fps expects 1..=240");
                    std::process::exit(2);
                }
            },
            "--no-share" => share = false,
            other => {
                eprintln!("hikari: unknown argument {}", other);
                std::process::exit(2);
            }
        }
    }
    let port = match &settings["port"] {
        _ if port_arg.is_some() => {
            let text = port_arg.unwrap_or_default();
            match text.parse::<u16>() {
                Ok(port) if port != 0 && text.bytes().all(|byte| byte.is_ascii_digit()) => port,
                _ => {
                    eprintln!("hikari: --port must be 1 to 65535, got {}", text);
                    std::process::exit(2);
                }
            }
        }
        Value::Null => 8793,
        value => match value.as_u64().and_then(|port| u16::try_from(port).ok()).filter(|port| *port != 0) {
            Some(port) => port,
            None => {
                eprintln!("hikari: port in settings.json must be 1-65535, got {}", value);
                std::process::exit(1);
            }
        },
    };
    let built_bg_dir = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../background"));
    let bg_dir = if built_bg_dir.is_dir() { built_bg_dir } else { exe_dir.join("background") };
    if settings_only {
        settings_tui::run(port, bg_dir);
        return;
    }
    let listener = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("hikari: can't listen on port {} ({}); another hikari may be running, or set a free port in settings.json", port, error);
            std::process::exit(1);
        }
    };
    if !no_settings {
        thread::spawn(move || settings_tui::open_window(&exe, port));
    }
    let (jobs, job_queue) = channel();
    let models = discover_models(&exe_dir);
    if models.is_empty() {
        eprintln!("hikari: no models in {}; copy a model folder there, for example Samples/Resources/Hiyori from the Cubism SDK", exe_dir.join("models").display());
        std::process::exit(1);
    }
    let built_defaults = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../default.model3.json"));
    let defaults_path = if exe_dir.join("default.model3.json").is_file() { exe_dir.join("default.model3.json") } else { built_defaults };
    let (shipped, default_actions) = vtube::defaults(&defaults_path).unwrap_or_else(|error| {
        eprintln!("hikari: no default actions or states, can't read {} ({})", defaults_path.display(), error);
        (Vec::new(), Vec::new())
    });
    let name = setting_model(&settings, &models);
    let setup = vtube::load(&models[&name]);
    let mut settings_physics = settings["physics"].as_object().cloned().unwrap_or_default();
    if settings.get("physics").is_some() && (!settings["physics"].is_object() || physics_changes(&settings_physics).is_err()) {
        println!("hikari: settings.json physics {} is not valid, using the defaults", settings["physics"]);
        settings_physics = serde_json::Map::new();
    }
    let model = describe_model(&name, &setup, &shipped, &default_actions, &settings_physics);
    let (idle_actions, action_every, unknown, banned) = parse_idle(settings.get("idle_actions"), settings.get("action_every"), &[], &json!(15), &model.actions).unwrap_or_else(|line| {
        eprintln!("{}", line);
        std::process::exit(1);
    });
    for name in banned {
        println!("hikari: {} can't be an idle action", name);
    }
    if !unknown.is_empty() {
        eprintln!("hikari: settings.json idle_actions skipped unknown {}", unknown.join(", "));
    }
    println!("hikari: idle actions={} action_every={}", idle_actions.join(","), action_every);
    let mut face = settings["face"].as_str().unwrap_or("neutral").to_string();
    if !settings["face"].is_null() && (face == "none" || !model.states.contains(&face)) {
        println!("hikari: settings.json face {} is not a state, using neutral", settings["face"]);
        face = "neutral".to_string();
    }
    let mut idle_loop = settings["idle_loop"].as_str().unwrap_or("").to_string();
    if !idle_loop.is_empty() && !setup.idle_loops.iter().any(|row| row.0 == idle_loop) || !settings["idle_loop"].is_null() && !settings["idle_loop"].is_string() {
        println!("hikari: settings.json idle_loop {} is not an idle motion, using the default", settings["idle_loop"]);
        idle_loop = String::new();
    }
    let transition_seconds = match read_transition_seconds(settings.get("transition_seconds")) {
        Ok(seconds) => seconds.unwrap_or(TRANSITION_SECONDS_DEFAULT),
        Err(other) => {
            println!("hikari: settings.json transition_seconds {} is not a number 0..{}, using {}", other, TRANSITION_SECONDS_MAX, TRANSITION_SECONDS_DEFAULT);
            TRANSITION_SECONDS_DEFAULT
        }
    };
    let now = Instant::now();
    let app = Arc::new(App {
        jobs,
        timer_queue: unsafe { kqueue() },
        ready: AtomicBool::new(false),
        model: Mutex::new(model),
        size: setting_size(&settings, "size", (960, 720)),
        idle: Mutex::new(IdleSettings { actions: idle_actions, every: action_every, face: face.clone(), idle_loop, generation: 1, path: settings_path }),
        models,
        broken: Mutex::new(Vec::new()),
        bg_lock: Mutex::new(()),
        settings_lock: Mutex::new(()),
        bg_dir,
        shipped,
        default_actions,
        views: Mutex::new(Views { live: HashMap::new(), saved: load_views(&exe_dir.join("views.json")), path: exe_dir.join("views.json") }),
        window_background: Mutex::new(None),
        mouths: Mutex::new(Mouths { owner: 0, t0: now, fps: 0.0, ring: [(usize::MAX, -1.0); MOUTHS] }),
        state: Mutex::new(ModelState { face, pose: "none".to_string(), face_given: false, generation: 1 }),
        asleep: AtomicBool::new(false),
        head_lead_ms: AtomicU64::new(0),
        params: Mutex::new(Vec::new()),
        physics: Mutex::new(None),
        settings_physics: Mutex::new(settings_physics),
        transition_seconds: AtomicU64::new(transition_seconds.to_bits()),
        shared: Mutex::new(Shared { engine: 0, name: String::new(), window: String::new(), pending: Vec::new(), bg_clock_ms: 0.0 }),
        picture: (Mutex::new(Picture { seq: 0, head: [0; 4], frames: HashMap::new(), wants: Vec::new(), window_fps: 0.0 }), Condvar::new()),
        lane_locks: Default::default(),
        rendering: Default::default(),
    });
    let render_app = app.clone();
    thread::spawn(move || run_render(render_app, job_queue, port));
    let picture_app = app.clone();
    thread::spawn(move || run_picture(picture_app));
    if !window {
        accept_connections(&app, listener);
        return;
    }
    let accept_app = app.clone();
    thread::spawn(move || accept_connections(&accept_app, listener));
    run_window(&app, app.size, fps, share);
}

fn accept_connections(app: &Arc<App>, listener: TcpListener) {
    for stream in listener.incoming().flatten() {
        let connection_app = app.clone();
        thread::spawn(move || handle_connection(connection_app, stream));
    }
}

extern "C" fn window_step(context: *mut c_void) {
    let frame = unsafe { &mut *context.cast::<WindowLoop>() };
    let mut shared = frame.app.shared.lock().unwrap_or_else(PoisonError::into_inner);
    let wanted = shared.name.clone();
    if wanted != frame.name {
        shared.pending.push(wanted.clone());
        drop(shared);
        let Some((mut lanes, _)) = build_lanes(frame.app, &wanted, &[frame.size], None) else {
            unpin(frame.app, &wanted);
            release_textures(frame.app, &wanted);
            thread::sleep(Duration::from_millis(10));
            return;
        };
        let old = std::mem::replace(&mut frame.state, lanes.remove(0));
        unsafe { engine_ffi::engine_destroy(old.engine) };
        let old_name = std::mem::replace(&mut frame.name, wanted);
        shared = frame.app.shared.lock().unwrap_or_else(PoisonError::into_inner);
        shared.window = frame.name.clone();
        drop(shared);
        unpin(frame.app, &frame.name);
        release_textures(frame.app, &old_name);
        println!("hikari: window model={}", frame.name);
        shared = frame.app.shared.lock().unwrap_or_else(PoisonError::into_inner);
    }
    if shared.name == frame.name {
        unsafe { engine_ffi::engine_mirror(frame.state.engine, shared.engine as *mut engine_ffi::Engine) };
    }
    let bg_clock_ms = shared.bg_clock_ms;
    drop(shared);
    let (mut pan_x, mut pan_y, mut zoom, mut cursor_x, mut cursor_y, mut turn, mut keys) = (0.0f32, 0.0f32, 1.0f32, 0.0f32, 0.0f32, 0.0f32, 0);
    let moved = unsafe { engine_ffi::window_input(frame.window, &mut pan_x, &mut pan_y, &mut zoom, &mut cursor_x, &mut cursor_y, &mut turn, &mut keys) };
    let mut views = frame.app.views.lock().unwrap_or_else(PoisonError::into_inner);
    let base = model_view(&views, &frame.name, frame.state.framing);
    let mut view = base;
    if moved {
        let rot = (base[3] + turn * WINDOW_TURN_DEGREES * frame.last.elapsed().as_secs_f32().min(0.1) + 180.0).rem_euclid(360.0) - 180.0;
        view = [base[0] * zoom, zoom * (base[1] + pan_x) + (1.0 - zoom) * cursor_x * 0.5, zoom * (base[2] + pan_y) + (1.0 - zoom) * cursor_y * 0.5, rot];
        if keys & 1 != 0 {
            view = resolve_view(MODEL_FRAMING, frame.state.framing);
        }
        if view != base {
            views.live.insert(frame.name.clone(), view);
            println!("hikari: view model={} zoom={} x={} y={} rot={}", frame.name, view[0], view[1], view[2], view[3]);
        }
        if keys & 2 != 0 {
            save_view(&mut views, &[(frame.name.clone(), view)]).ok();
        }
    }
    drop(views);
    unsafe { engine_ffi::engine_view_set(frame.state.engine, view[0], view[1], view[2], view[3]) };
    let background = frame.app.window_background.lock().unwrap_or_else(PoisonError::into_inner).take();
    if let Some((bytes, content_type)) = background {
        load_background(&[frame.state.engine], bytes.as_deref().map(Vec::as_slice), &content_type);
    }
    let now = Instant::now();
    let mouth = frame.app.mouths.lock().unwrap_or_else(PoisonError::into_inner).mouth_now(now);
    frame.last = now;
    if mouth >= 0.0 {
        frame.mouth_frames += 1;
        frame.mouth_max = frame.mouth_max.max(mouth);
    }
    if unsafe { engine_ffi::engine_present(frame.state.engine, frame.window, bg_clock_ms as f32, frame.frame_seconds, frame.share) } {
        frame.frames += 1;
    } else {
        thread::sleep(Duration::from_millis(10));
    }
    if now - frame.second >= Duration::from_secs(1) {
        println!("hikari: window size={}x{} fps={}", frame.width, frame.height, frame.frames);
        if frame.mouth_frames > 0 {
            println!("hikari: window mouth frames={} open_max={:.3}", frame.mouth_frames, frame.mouth_max);
        }
        frame.frames = 0;
        frame.mouth_frames = 0;
        frame.mouth_max = 0.0;
        frame.second = now;
    }
}

fn run_window(app: &App, size: (usize, usize), fps: u32, share: bool) {
    while !app.ready.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(10));
    }
    let name = app.model.lock().unwrap_or_else(PoisonError::into_inner).name.clone();
    app.shared.lock().unwrap_or_else(PoisonError::into_inner).pending.push(name.clone());
    let Some((mut lanes, _)) = build_lanes(app, &name, &[size], None) else {
        eprintln!("hikari: window model {} unavailable", name);
        std::process::exit(1);
    };
    let mut shared = app.shared.lock().unwrap_or_else(PoisonError::into_inner);
    shared.window = name.clone();
    drop(shared);
    unpin(app, &name);
    let window = unsafe { engine_ffi::window_open(c"hikari".as_ptr(), size.0 as i32, size.1 as i32) };
    if window.is_null() {
        eprintln!("hikari: window_open failed");
        std::process::exit(1);
    }
    app.picture.0.lock().unwrap_or_else(PoisonError::into_inner).window_fps = fps as f64;
    let now = Instant::now();
    let mut frame = WindowLoop { app, name, size, state: lanes.remove(0), window, frame_seconds: 1.0 / fps as f32, last: now, second: now, frames: 0, mouth_frames: 0, mouth_max: 0.0, width: 0, height: 0, share };
    let frame_ptr = std::ptr::addr_of_mut!(frame);
    loop {
        while unsafe { engine_ffi::window_poll(window, std::ptr::addr_of_mut!((*frame_ptr).width), std::ptr::addr_of_mut!((*frame_ptr).height), window_step, frame_ptr.cast()) } {
            window_step(frame_ptr.cast());
        }
        let (name, framing) = unsafe { ((*frame_ptr).name.clone(), (*frame_ptr).state.framing) };
        let views = app.views.lock().unwrap_or_else(PoisonError::into_inner);
        let view = model_view(&views, &name, framing);
        let saved = views.saved.get(&name).copied().unwrap_or_else(|| resolve_view(MODEL_FRAMING, framing));
        drop(views);
        if view == saved {
            break;
        }
        let model = CString::new(name.as_str()).unwrap_or_default();
        match unsafe { engine_ffi::window_ask_save(window, model.as_ptr()) } {
            1 => {
                if save_view(&mut app.views.lock().unwrap_or_else(PoisonError::into_inner), &[(name, view)]).is_ok() {
                    break;
                }
            }
            0 => break,
            _ => {}
        }
    }
    unsafe {
        engine_ffi::window_close(window);
        engine_ffi::engine_destroy((*frame_ptr).state.engine);
    }
    std::process::exit(0);
}
