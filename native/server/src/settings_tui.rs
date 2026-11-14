use crate::PHYSICS_DEFAULT;
use serde_json::{json, Value};
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;

const ROWS: [&str; 18] = ["model", "face", "background", "state now", "pose now", "play action", "idle actions", "every, at least", "every, at most", "transition", "zoom", "x", "y", "rotation", "physics", "wind", "wind from", "gravity from"];
const SECTIONS: [(usize, &str); 4] = [(0, "look"), (6, "idle"), (10, "framing"), (14, "physics")];
const LIST_ROWS: usize = 7;
const CHECKLIST_ROW: usize = 6;
const FIRST_EVERY_ROW: usize = 7;
const FIRST_VIEW_ROW: usize = 10;
const FIRST_PHYSICS_ROW: usize = 14;
const VIEW_STEPS: [f32; 4] = [0.05, 0.02, 0.02, 5.0];
const IMAGE_TYPES: [&str; 4] = ["png", "jpg", "jpeg", "gif"];
const PHYSICS_PATHS: [(&str, &str); 4] = [("strength", ""), ("wind", "strength"), ("wind", "direction"), ("gravity", "direction")];
const COMPASS: [&str; 8] = ["the top", "top right", "the right", "bottom right", "the bottom", "bottom left", "the left", "top left"];
const FLOW: [&str; 8] = ["↓", "↙", "←", "↖", "↑", "↗", "→", "↘"];
const PAGE: usize = 12;
const BAR: usize = 24;
const LABEL_WIDTH: usize = 16;
const TITLE: &str = "\x1b[1;33m";
const CURSOR: &str = "\x1b[1;32m";
const DIM: &str = "\x1b[2m";
const HEADING: &str = "\x1b[1m";
const UNSAVED: &str = "\x1b[35m";
const GOOD: &str = "\x1b[32m";
const BAD: &str = "\x1b[31m";
const RESET: &str = "\x1b[0m";

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn GetStdHandle(id: u32) -> *mut std::ffi::c_void;
    fn GetConsoleMode(handle: *mut std::ffi::c_void, mode: *mut u32) -> i32;
    fn SetConsoleMode(handle: *mut std::ffi::c_void, mode: u32) -> i32;
}

enum Key {
    Up,
    Down,
    Left,
    Right,
    Space,
    Enter,
    Escape,
    Quit,
    Other,
}

#[derive(Default)]
struct Tui {
    port: u16,
    saved: Value,
    model: String,
    face: String,
    held: String,
    pose: String,
    background: String,
    backgrounds: Vec<String>,
    bg_dir: PathBuf,
    view: [f32; 4],
    saved_view: [f32; 4],
    view_touched: bool,
    left_views: std::collections::BTreeMap<String, [f32; 4]>,
    idle: Vec<String>,
    every: [u64; 2],
    transition_tenths: u64,
    physics: [f32; 4],
    live_physics: [f32; 4],
    models: Vec<String>,
    faces: Vec<String>,
    actions: Vec<String>,
    played: String,
    row: usize,
    open: bool,
    pick: usize,
    checked: Vec<String>,
    status: String,
    status_color: &'static str,
}

pub fn open_window(exe: &Path, port: u16) {
    let port = port.to_string();
    #[cfg(target_os = "macos")]
    let opened = {
        use std::os::unix::fs::PermissionsExt;
        let script = std::env::temp_dir().join(format!("hikari-settings-{}.command", port));
        let quoted = exe.to_string_lossy().replace('\'', "'\\''");
        let close = "osascript -e 'on run argv' -e 'tell application \"Terminal\"' -e 'repeat with terminal_window in windows' -e 'if tty of selected tab of terminal_window is item 1 of argv then close terminal_window saving no' -e 'end repeat' -e 'end tell' -e 'end run' \"$window_tty\"";
        std::fs::write(&script, format!("#!/bin/sh\nrm -f \"$0\"\nwindow_tty=$(tty)\n'{}' --settings --port {}\nnohup {} >/dev/null 2>&1 &\n", quoted, port, close)).is_ok()
            && std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).is_ok()
            && matches!(Command::new("open").arg(&script).status(), Ok(status) if status.success())
    };
    #[cfg(windows)]
    let opened = {
        use std::os::windows::process::CommandExt;
        Command::new(exe).args(["--settings", "--port", &port]).creation_flags(0x10).spawn().is_ok()
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let opened = {
        let mut opened = false;
        for (terminal, flag) in [("x-terminal-emulator", "-e"), ("gnome-terminal", "--"), ("konsole", "-e"), ("xfce4-terminal", "-x"), ("xterm", "-e")] {
            if matches!(Command::new(terminal).arg(flag).arg(exe).args(["--settings", "--port", &port]).status(), Ok(status) if status.success()) {
                opened = true;
                break;
            }
        }
        opened
    };
    if !opened {
        println!("hikari: couldn't open a terminal for settings, run hikari --settings --port {} in one", port);
    }
}

fn request(port: u16, method: &str, path: &str, body: &[u8]) -> io::Result<(u16, String)> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    stream.set_read_timeout(Some(Duration::from_secs(60)))?;
    write!(stream, "{} {} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", method, path, body.len())?;
    stream.write_all(body)?;
    let mut text = String::new();
    stream.read_to_string(&mut text)?;
    let (head, body) = text.split_once("\r\n\r\n").ok_or(io::ErrorKind::InvalidData)?;
    let status = match head.split(' ').nth(1) {
        Some(code) => match code.parse() {
            Ok(status) => status,
            Err(_) => return Err(io::ErrorKind::InvalidData.into()),
        },
        None => return Err(io::ErrorKind::InvalidData.into()),
    };
    Ok((status, body.to_string()))
}

fn get_json(port: u16, method: &str, path: &str, body: &str) -> io::Result<Value> {
    match request(port, method, path, body.as_bytes())? {
        (200, text) => match serde_json::from_str(&text) {
            Ok(value) => Ok(value),
            Err(_) => Err(io::ErrorKind::InvalidData.into()),
        },
        (status, text) => Err(io::Error::other(format!("{} {} returned {} {}", method, path, status, text.trim()))),
    }
}

fn query_value(text: &str) -> String {
    let mut encoded = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{:02X}", byte));
        }
    }
    encoded
}

fn slider(value: f32, max: f32) -> String {
    let filled = ((value / max).clamp(0.0, 1.0) * BAR as f32).round() as usize;
    format!("{}{}{}{}{} {}", HEADING, "━".repeat(filled), RESET, DIM, "─".repeat(BAR - filled), RESET) + &value.to_string()
}

fn compass(degrees: f32) -> String {
    let point = ((degrees.rem_euclid(360.0) / 45.0).round() as usize) % 8;
    format!("{} from {} {}· {}°{}", FLOW[point], COMPASS[point], DIM, degrees, RESET)
}

fn raw_mode(on: bool, saved: &str) -> String {
    #[cfg(unix)]
    {
        let args: &[&str] = if on { &["-g"] } else { &[saved] };
        let old = match Command::new("stty").args(args).stdin(std::process::Stdio::inherit()).output() {
            Ok(out) => String::from_utf8_lossy(&out.stdout).trim().to_string(),
            Err(_) => String::new(),
        };
        if on {
            Command::new("stty").args(["raw", "-echo"]).stdin(std::process::Stdio::inherit()).status().ok();
        }
        old
    }
    #[cfg(windows)]
    unsafe {
        let input = GetStdHandle(0xFFFF_FFF6);
        let output = GetStdHandle(0xFFFF_FFF5);
        let (mut input_mode, mut output_mode) = (0u32, 0u32);
        GetConsoleMode(input, &mut input_mode);
        GetConsoleMode(output, &mut output_mode);
        if on {
            SetConsoleMode(input, 0x200);
            SetConsoleMode(output, output_mode | 0x5);
            return format!("{} {}", input_mode, output_mode);
        }
        let mut modes = Vec::new();
        for mode in saved.split(' ') {
            if let Ok(mode) = mode.parse() {
                modes.push(mode);
            }
        }
        let mut modes = modes.into_iter();
        SetConsoleMode(input, modes.next().unwrap_or(input_mode));
        SetConsoleMode(output, modes.next().unwrap_or(output_mode));
        String::new()
    }
}

fn read_key(bytes: &Receiver<u8>, timeout: Duration) -> Result<Key, RecvTimeoutError> {
    let key = match bytes.recv_timeout(timeout)? {
        27 => match bytes.recv_timeout(Duration::from_millis(30)) {
            Ok(b'[') | Ok(b'O') => match bytes.recv_timeout(Duration::from_millis(30)) {
                Ok(b'A') => Key::Up,
                Ok(b'B') => Key::Down,
                Ok(b'C') => Key::Right,
                Ok(b'D') => Key::Left,
                _ => Key::Other,
            },
            _ => Key::Escape,
        },
        b'\r' | b'\n' => Key::Enter,
        b' ' => Key::Space,
        b'q' | 3 => Key::Quit,
        _ => Key::Other,
    };
    Ok(key)
}

impl Tui {
    fn load(&mut self) -> io::Result<()> {
        let saved = get_json(self.port, "GET", "/settings", "")?;
        self.models = serde_json::from_value(get_json(self.port, "GET", "/models", "")?["models"].take()).unwrap_or_default();
        let mut state = get_json(self.port, "GET", "/state", "")?;
        let faces: Vec<String> = serde_json::from_value(state["states"].take()).unwrap_or_default();
        self.faces.clear();
        for name in faces {
            if name != "none" {
                self.faces.push(name);
            }
        }
        self.held = state["face"].as_str().unwrap_or("none").to_string();
        self.pose = state["pose"].as_str().unwrap_or("none").to_string();
        self.read_background();
        if self.view_touched && self.view != self.saved_view {
            self.left_views.entry(self.saved["model"].as_str().unwrap_or_default().to_string()).or_insert(self.saved_view);
        }
        self.view_touched = false;
        let model = saved["model"].as_str().unwrap_or_default();
        if let Some(view) = self.left_views.get(model).copied() {
            if matches!(self.put_view(view, false), Ok((204, _))) {
                self.left_views.remove(model);
            }
        }
        self.read_view();
        self.actions = serde_json::from_value(get_json(self.port, "GET", "/action", "")?["actions"].take()).unwrap_or_default();
        self.live_physics = crate::effective_physics(None, &crate::physics_changes(get_json(self.port, "POST", "/physics", "{}")?.as_object().unwrap_or(&serde_json::Map::new())).unwrap_or_default());
        self.saved = saved;
        self.reset();
        Ok(())
    }

    fn read_state(&mut self) {
        if let Ok(state) = get_json(self.port, "GET", "/state", "") {
            self.held = state["face"].as_str().unwrap_or("none").to_string();
            self.pose = state["pose"].as_str().unwrap_or("none").to_string();
        }
    }

    fn read_background(&mut self) {
        if let Ok(meta) = get_json(self.port, "GET", "/background", "") {
            self.background = if meta["kind"] == "none" { "none".to_string() } else { meta["name"].as_str().unwrap_or_default().to_string() };
        }
    }

    fn list_backgrounds(&mut self) {
        let mut names = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&self.bg_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if !name.starts_with("current.") && matches!(name.rsplit_once('.'), Some((_, ext)) if IMAGE_TYPES.contains(&ext.to_ascii_lowercase().as_str())) {
                    names.push(name);
                }
            }
        }
        names.sort();
        self.backgrounds = [vec!["none".to_string()], names].concat();
    }

    fn read_view(&mut self) {
        if let Ok(view) = get_json(self.port, "GET", "/view", "") {
            let saved = if view["saved"].is_null() { &view["default"] } else { &view["saved"] };
            for (index, name) in ["zoom", "x", "y", "rot"].iter().enumerate() {
                self.view[index] = view[name].as_f64().unwrap_or(0.0) as f32;
                self.saved_view[index] = saved[name].as_f64().unwrap_or(0.0) as f32;
            }
        }
    }

    fn put_view(&self, view: [f32; 4], save: bool) -> io::Result<(u16, String)> {
        let path = format!("/view?zoom={}&x={}&y={}&rot={}&save={}", view[0], view[1], view[2], view[3], save);
        request(self.port, "PUT", &path, b"")
    }

    fn restore_picks(&mut self) {
        self.model = self.saved["model"].as_str().unwrap_or_default().to_string();
        self.face = self.saved["face"].as_str().unwrap_or_default().to_string();
        self.idle = serde_json::from_value(self.saved["idle_actions"].clone()).unwrap_or_default();
    }

    fn reset(&mut self) {
        self.restore_picks();
        self.every = match crate::parse_every(&self.saved["action_every"]) {
            Ok((min, max)) => [min, max],
            Err(_) => [1, 1],
        };
        self.transition_tenths = (self.saved["transition_seconds"].as_f64().unwrap_or(0.0) * 10.0).round() as u64;
        self.physics = self.live_physics;
    }

    fn changes(&self) -> serde_json::Map<String, Value> {
        let mut changes = serde_json::Map::new();
        let every = if self.every[0] == self.every[1] { json!(self.every[0]) } else { json!(self.every) };
        if self.saved["action_every"] != every {
            changes.insert("action_every".to_string(), every);
        }
        if (self.saved["transition_seconds"].as_f64().unwrap_or(0.0) * 10.0).round() as u64 != self.transition_tenths {
            changes.insert("transition_seconds".to_string(), json!(self.transition_tenths as f64 / 10.0));
        }
        if self.physics != self.live_physics {
            let mut physics = self.saved["physics"].clone();
            for (index, (outer, inner)) in PHYSICS_PATHS.iter().enumerate() {
                if self.physics[index] == self.live_physics[index] {
                    continue;
                }
                if inner.is_empty() {
                    physics[outer] = json!(self.physics[index]);
                } else {
                    physics[outer][inner] = json!(self.physics[index]);
                }
            }
            changes.insert("physics".to_string(), physics);
        }
        changes
    }

    fn unsaved(&self, row: usize, changes: &serde_json::Map<String, Value>) -> bool {
        match row {
            7 | 8 => changes.contains_key("action_every"),
            9 => changes.contains_key("transition_seconds"),
            10..=13 => self.view[row - FIRST_VIEW_ROW] != self.saved_view[row - FIRST_VIEW_ROW],
            row if row >= FIRST_PHYSICS_ROW => self.physics[row - FIRST_PHYSICS_ROW] != self.live_physics[row - FIRST_PHYSICS_ROW],
            _ => false,
        }
    }

    fn preview_physics(&self, values: [f32; 4]) {
        let body = json!({ "strength": values[0], "wind": { "strength": values[1], "direction": values[2] }, "gravity": { "direction": values[3] } });
        request(self.port, "POST", "/physics", body.to_string().as_bytes()).ok();
    }

    fn put(&mut self, changes: serde_json::Map<String, Value>, done: String) {
        (self.status, self.status_color) = ("saving...".to_string(), DIM);
        self.draw();
        let reload = changes.contains_key("model");
        let saves_physics = changes.contains_key("physics");
        (self.status, self.status_color) = match request(self.port, "PUT", "/settings", Value::Object(changes).to_string().as_bytes()) {
            Ok((200, text)) if !reload => {
                self.saved = serde_json::from_str(&text).unwrap_or(Value::Null);
                if saves_physics {
                    self.live_physics = self.physics;
                }
                (format!("✓ {}", done), GOOD)
            }
            Ok((200, _)) => match self.load() {
                Ok(()) => (format!("✓ {}", done), GOOD),
                Err(error) => (format!("✗ {}", error), BAD),
            },
            Ok((_, text)) => (format!("✗ {}", text.trim()), BAD),
            Err(error) => (format!("✗ {}", error), BAD),
        };
        if self.status_color == BAD {
            self.restore_picks();
        }
    }

    fn save(&mut self) {
        let changes = self.changes();
        let framing = self.view != self.saved_view;
        if changes.is_empty() && !framing {
            (self.status, self.status_color) = ("nothing to save".to_string(), DIM);
            return;
        }
        if framing {
            (self.status, self.status_color) = match self.put_view(self.view, true) {
                Ok((204, _)) => {
                    self.saved_view = self.view;
                    self.view_touched = false;
                    ("✓ framing saved to views.json".to_string(), GOOD)
                }
                Ok((status, text)) => (format!("✗ {} {}", status, text.trim()), BAD),
                Err(error) => (format!("✗ {}", error), BAD),
            };
            if self.status_color == BAD {
                return;
            }
        }
        if !changes.is_empty() {
            self.put(changes, "saved to settings.json".to_string());
        }
    }

    fn rollback_view(&mut self) -> bool {
        if !self.view_touched || self.view == self.saved_view {
            self.view_touched = false;
            return true;
        }
        match self.put_view(self.saved_view, false) {
            Ok((204, _)) => {
                self.view = self.saved_view;
                self.view_touched = false;
                true
            }
            Ok((status, text)) => {
                (self.status, self.status_color) = (format!("✗ framing not reverted: {} {}", status, text.trim()), BAD);
                false
            }
            Err(error) => {
                (self.status, self.status_color) = (format!("✗ framing not reverted: {}", error), BAD);
                false
            }
        }
    }

    fn revert(&mut self) {
        if self.physics != self.live_physics {
            self.preview_physics(self.live_physics);
        }
        self.reset();
        if self.rollback_view() {
            (self.status, self.status_color) = ("reverted".to_string(), DIM);
        }
    }

    fn items(&self) -> Vec<String> {
        match self.row {
            0 => self.models.clone(),
            1 => self.faces.clone(),
            2 => self.backgrounds.clone(),
            3 | 4 => [vec!["none".to_string()], self.faces.clone()].concat(),
            5 => self.actions.clone(),
            _ => self.actions.iter().filter(|name| *name != "sleep").cloned().collect(),
        }
    }

    fn current(&self) -> &str {
        match self.row {
            0 => &self.model,
            1 => &self.face,
            2 => &self.background,
            3 => &self.held,
            4 => &self.pose,
            _ => &self.played,
        }
    }

    fn choose(&mut self, items: &[String]) {
        let Some(name) = items.get(self.pick).cloned() else {
            self.open = false;
            return;
        };
        if self.row == CHECKLIST_ROW {
            if let Some(index) = self.checked.iter().position(|idle| *idle == name) {
                self.checked.remove(index);
            } else {
                self.checked.push(name);
            }
            return;
        }
        self.open = false;
        let (path, done) = match self.row {
            0 => {
                if !self.rollback_view() {
                    return;
                }
                self.model = name.clone();
                return self.put(serde_json::Map::from_iter([("model".to_string(), json!(name))]), format!("switched to {}", name));
            }
            1 => {
                self.face = name.clone();
                return self.put(serde_json::Map::from_iter([("face".to_string(), json!(name))]), format!("face is {}", name));
            }
            2 => return self.choose_background(name),
            3 => (format!("/state?face={}", query_value(&name)), format!("✓ holding {}, not saved", name)),
            4 => (format!("/state?pose={}", query_value(&name)), format!("✓ pose {}, not saved", name)),
            _ => (format!("/action?action={}", query_value(&name)), format!("▸ playing {}", name)),
        };
        (self.status, self.status_color) = match request(self.port, "POST", &path, b"") {
            Ok((200, _)) => {
                match self.row {
                    3 => self.held = name,
                    4 => self.pose = name,
                    _ => self.played = name,
                }
                (done, GOOD)
            }
            Ok((_, text)) => (format!("✗ {}", text.trim()), BAD),
            Err(error) => (format!("✗ {}", error), BAD),
        };
    }

    fn choose_background(&mut self, name: String) {
        let sent = if name == "none" {
            request(self.port, "DELETE", "/background", b"")
        } else {
            match std::fs::read(self.bg_dir.join(&name)) {
                Ok(bytes) => request(self.port, "PUT", &format!("/background?name={}", query_value(&name)), &bytes),
                Err(error) => Err(error),
            }
        };
        (self.status, self.status_color) = match sent {
            Ok((200, _)) => {
                self.background = name;
                (format!("✓ background is {}", self.background), GOOD)
            }
            Ok((413, _)) => ("✗ that image is over 5 MB".to_string(), BAD),
            Ok((status, text)) => (format!("✗ {} {}", status, text.trim()), BAD),
            Err(error) => (format!("✗ {}", error), BAD),
        };
    }

    fn finish_checklist(&mut self) {
        self.open = false;
        let mut old = self.idle.clone();
        let mut new = self.checked.clone();
        old.sort();
        new.sort();
        if old == new {
            return;
        }
        self.idle = self.checked.clone();
        let done = format!("{} idle actions on", self.idle.len());
        self.put(serde_json::Map::from_iter([("idle_actions".to_string(), json!(self.idle))]), done);
    }

    fn step(&mut self, direction: i64) {
        match self.row {
            7 | 8 => {
                let index = self.row - FIRST_EVERY_ROW;
                self.every[index] = (self.every[index] as i64 + direction).max(1) as u64;
                if self.every[0] > self.every[1] {
                    self.every[1 - index] = self.every[index];
                }
            }
            9 => self.transition_tenths = (self.transition_tenths as i64 + direction).clamp(0, (crate::TRANSITION_SECONDS_MAX * 10.0).round() as i64) as u64,
            10..=13 => {
                let index = self.row - FIRST_VIEW_ROW;
                let moved = self.view[index] + VIEW_STEPS[index] * direction as f32;
                self.view[index] = match index {
                    0 => (moved.max(VIEW_STEPS[0]) * 100.0).round() / 100.0,
                    3 => ((moved + 180.0).rem_euclid(360.0) - 180.0).round(),
                    _ => (moved * 100.0).round() / 100.0,
                };
                self.view_touched = true;
                self.put_view(self.view, false).ok();
            }
            row if row >= FIRST_PHYSICS_ROW => {
                let index = row - FIRST_PHYSICS_ROW;
                let value = &mut self.physics[index];
                *value = if index >= 2 { (*value + 5.0 * direction as f32).rem_euclid(360.0) } else { (*value + direction as f32).clamp(0.0, 100.0) };
                self.preview_physics(self.physics);
            }
            _ => {}
        }
    }

    fn value_text(&self, row: usize) -> String {
        match row {
            0 => format!("{}  {}▾{}", self.model, DIM, RESET),
            1 => format!("{}  {}▾{}", self.face, DIM, RESET),
            2 => format!("{}  {}▾{}", self.background, DIM, RESET),
            3 => format!("{}  {}▾  live, not saved{}", self.held, DIM, RESET),
            4 => format!("{}  {}▾  live, not saved{}", self.pose, DIM, RESET),
            5 => format!("{}  {}▸  plays now{}", if self.played.is_empty() { "pick one" } else { &self.played }, DIM, RESET),
            6 => format!("{} of {} on  {}▾{}", self.idle.len(), self.actions.iter().filter(|name| *name != "sleep").count(), DIM, RESET),
            7 | 8 => format!("{}‹{} {} s {}›{}", DIM, RESET, self.every[row - FIRST_EVERY_ROW], DIM, RESET),
            9 => format!("{}‹{} {} s {}›{}", DIM, RESET, self.transition_tenths as f64 / 10.0, DIM, RESET),
            10 => format!("{}‹{} {}× {}›{}", DIM, RESET, self.view[0], DIM, RESET),
            11 | 12 => format!("{}‹{} {} {}›{}", DIM, RESET, self.view[row - FIRST_VIEW_ROW], DIM, RESET),
            13 => format!("{}‹{} {}° {}›{}", DIM, RESET, self.view[3], DIM, RESET),
            14 | 15 => slider(self.physics[row - FIRST_PHYSICS_ROW], 100.0),
            _ => compass(self.physics[row - FIRST_PHYSICS_ROW]),
        }
    }

    fn draw(&self) {
        let mut screen = String::from("\x1b[H\x1b[2J");
        if self.open {
            let checklist = self.row == CHECKLIST_ROW;
            let hint = if checklist { "↑↓ navigate  SPACE toggle  ENTER save  ESC back" } else { "↑↓ navigate  ENTER select  ESC back" };
            screen.push_str(&format!(" {}{}{}\r\n {}{}{}\r\n", TITLE, ROWS[self.row], RESET, DIM, hint, RESET));
            if self.row == 2 {
                screen.push_str(&format!(" {}png, jpeg or gif files in {}{}\r\n", DIM, self.bg_dir.display(), RESET));
            }
            screen.push_str("\r\n");
            let items = self.items();
            let start = self.pick.saturating_sub(PAGE - 1).min(items.len().saturating_sub(PAGE));
            if start > 0 {
                screen.push_str(&format!("     {}↑ {} more{}\r\n", DIM, start, RESET));
            }
            for (index, name) in items.iter().enumerate().skip(start).take(PAGE) {
                let mark = match (checklist, self.checked.contains(name), name == self.current()) {
                    (true, true, _) => "[✓]",
                    (true, false, _) => "[ ]",
                    (false, _, true) => "(●)",
                    (false, _, false) => "(○)",
                };
                let line = if index == self.pick { format!(" {}→ {} {}{}\r\n", CURSOR, mark, name, RESET) } else { format!("   {} {}\r\n", mark, name) };
                screen.push_str(&line);
            }
            if start + PAGE < items.len() {
                screen.push_str(&format!("     {}↓ {} more{}\r\n", DIM, items.len() - start - PAGE, RESET));
            }
        } else {
            screen.push_str(&format!(" {}hikari settings{}  {}127.0.0.1:{}{}\r\n", TITLE, RESET, DIM, self.port, RESET));
            let hint = if self.row < LIST_ROWS { "↑↓ navigate  ENTER open  Q quit" } else { "↑↓ navigate  ←→ adjust  ENTER save  ESC revert  Q quit" };
            screen.push_str(&format!(" {}{}{}\r\n", DIM, hint, RESET));
            let changes = self.changes();
            let mut pending = 0;
            for (row, label) in ROWS.iter().enumerate() {
                for (first, section) in SECTIONS {
                    if first == row {
                        screen.push_str(&format!("\r\n {}{}{}\r\n", HEADING, section, RESET));
                        break;
                    }
                }
                let unsaved = self.unsaved(row, &changes);
                pending += unsaved as usize;
                let mark = if unsaved { format!("{}•{}", UNSAVED, RESET) } else { " ".to_string() };
                let label = format!("{:width$}", label, width = LABEL_WIDTH);
                let lead = if row == self.row { format!(" {}→ {}{}", CURSOR, label, RESET) } else { format!("   {}", label) };
                screen.push_str(&format!("{}{} {}\r\n", lead, mark, self.value_text(row)));
            }
            let size = format!("{}×{}", self.saved["size"][0], self.saved["size"][1]);
            let idle_loop = match self.saved["idle_loop"].as_str() {
                Some(name) if !name.is_empty() => name,
                _ => "default",
            };
            screen.push_str(&format!("\r\n {}restart to change: port {} · size {} · idle loop {}{}\r\n", DIM, self.port, size, idle_loop, RESET));
            if pending > 0 && self.status.is_empty() {
                screen.push_str(&format!("\r\n {}• {} unsaved, ENTER saves, ESC reverts{}\r\n", UNSAVED, pending, RESET));
            }
        }
        if !self.status.is_empty() {
            screen.push_str(&format!("\r\n {}{}{}\r\n", self.status_color, self.status, RESET));
        }
        let mut out = io::stdout();
        out.write_all(screen.as_bytes()).ok();
        out.flush().ok();
    }

    fn key(&mut self, key: Key) {
        if self.open {
            let items = self.items();
            let count = items.len().max(1);
            match key {
                Key::Up => self.pick = (self.pick + count - 1) % count,
                Key::Down => self.pick = (self.pick + 1) % count,
                Key::Enter if self.row == CHECKLIST_ROW => self.finish_checklist(),
                Key::Space | Key::Enter => self.choose(&items),
                Key::Escape | Key::Left => self.open = false,
                _ => {}
            }
            return;
        }
        self.status.clear();
        match key {
            Key::Up => self.row = (self.row + ROWS.len() - 1) % ROWS.len(),
            Key::Down => self.row = (self.row + 1) % ROWS.len(),
            Key::Left => self.step(-1),
            Key::Right => self.step(1),
            Key::Space | Key::Enter if self.row < LIST_ROWS => {
                self.read_state();
                if self.row == 2 {
                    self.read_background();
                    self.list_backgrounds();
                }
                let items = self.items();
                self.pick = 0;
                for (index, name) in items.iter().enumerate() {
                    if name == self.current() {
                        self.pick = index;
                        break;
                    }
                }
                self.checked = self.idle.clone();
                self.open = true;
            }
            Key::Enter => self.save(),
            Key::Escape => self.revert(),
            _ => {}
        }
    }
}

pub fn run(port: u16, bg_dir: PathBuf) {
    let mut tui = Tui { port, bg_dir, every: [1, 1], physics: PHYSICS_DEFAULT, live_physics: PHYSICS_DEFAULT, status_color: DIM, ..Default::default() };
    println!("hikari: waiting for the renderer on 127.0.0.1:{}", port);
    loop {
        match request(port, "GET", "/models", b"") {
            Ok((200, _)) => break,
            Ok(_) => thread::sleep(Duration::from_millis(250)),
            Err(error) => {
                eprintln!("hikari: no renderer on 127.0.0.1:{} ({})", port, error);
                std::process::exit(1);
            }
        }
    }
    if let Err(error) = tui.load() {
        eprintln!("hikari: {}", error);
        std::process::exit(1);
    }
    let (sender, bytes) = channel();
    thread::spawn(move || {
        let mut stdin = io::stdin();
        let mut byte = [0u8; 1];
        while matches!(stdin.read(&mut byte), Ok(1)) && sender.send(byte[0]).is_ok() {}
    });
    let terminal = raw_mode(true, "");
    print!("\x1b[8;32;88t\x1b[?1049h\x1b[?25l");
    let mut gone = None;
    tui.draw();
    loop {
        match read_key(&bytes, Duration::from_secs(1)) {
            Ok(Key::Quit) | Err(RecvTimeoutError::Disconnected) => {
                tui.revert();
                break;
            }
            Ok(key) => {
                tui.key(key);
                tui.draw();
            }
            Err(RecvTimeoutError::Timeout) => {
                let shown = format!("{:?}{}{}{}{:?}{}", tui.saved, tui.held, tui.pose, tui.background, tui.view, tui.status);
                match get_json(port, "GET", "/settings", "") {
                    Ok(settings) if settings != tui.saved => {
                        let pending = tui.changes();
                        let (every, physics, transition_tenths) = (tui.every, tui.physics, tui.transition_tenths);
                        let same_model = settings["model"] == tui.saved["model"];
                        let reloaded = if same_model {
                            if !pending.contains_key("physics") {
                                if let Ok(live) = get_json(port, "POST", "/physics", "{}") {
                                    tui.live_physics = crate::effective_physics(None, &crate::physics_changes(live.as_object().unwrap_or(&serde_json::Map::new())).unwrap_or_default());
                                }
                            }
                            tui.saved = settings;
                            tui.reset();
                            Ok(())
                        } else {
                            tui.open = false;
                            tui.load()
                        };
                        if let Err(error) = reloaded {
                            (tui.status, tui.status_color) = (format!("✗ {}", error), BAD);
                        }
                        if pending.contains_key("action_every") {
                            tui.every = every;
                        }
                        if pending.contains_key("transition_seconds") {
                            tui.transition_tenths = transition_tenths;
                        }
                        if same_model && pending.contains_key("physics") {
                            tui.physics = physics;
                        }
                    }
                    Ok(_) => {
                        tui.read_state();
                        tui.read_background();
                        if !tui.view_touched {
                            tui.read_view();
                        }
                    }
                    Err(error) => {
                        gone = Some(error);
                        break;
                    }
                }
                if format!("{:?}{}{}{}{:?}{}", tui.saved, tui.held, tui.pose, tui.background, tui.view, tui.status) != shown {
                    tui.draw();
                }
            }
        }
    }
    print!("\x1b[0m\x1b[?25h\x1b[?1049l");
    io::stdout().flush().ok();
    raw_mode(false, &terminal);
    if tui.status_color == BAD {
        println!("hikari: {}", tui.status);
    }
    for model in tui.left_views.keys() {
        println!("hikari: unsaved framing on {} stays until the renderer restarts", model);
    }
    if let Some(error) = gone {
        println!("hikari: renderer on 127.0.0.1:{} stopped ({})", port, error);
    }
}
