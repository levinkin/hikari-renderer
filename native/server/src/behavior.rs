use crate::engine_ffi;
use crate::vtube::VtubeSetup;
use serde_json::{json, Value};
use std::ffi::{c_char, c_void, CStr, CString};
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::path::PathBuf;

static VTS_INPUTS: [(&CStr, &str); 12] = [
    (c"ParamAngleX", "FaceAngleX"),
    (c"ParamAngleY", "FaceAngleY"),
    (c"ParamAngleZ", "FaceAngleZ"),
    (c"ParamBodyAngleX", "FaceAngleX"),
    (c"ParamEyeLOpen", "EyeOpenLeft"),
    (c"ParamEyeROpen", "EyeOpenRight"),
    (c"ParamEyeBallX", "EyeRightX"),
    (c"ParamEyeBallY", "EyeRightY"),
    (c"ParamMouthOpenY", "MouthOpen"),
    (c"ParamMouthForm", "MouthSmile"),
    (c"ParamBrowLY", "Brows"),
    (c"ParamBrowRY", "Brows"),
];

const YAW_PER_ANGLE_X: f64 = 0.6;
const PITCH_PER_ANGLE_Y: f64 = 0.5;
const PITCH_HOME: f64 = 150.0;
const HEAD_SMOOTH_MS: f64 = 80.0;
const HEAD_TURN_SCALE: f32 = 0.6;

pub struct Behavior {
    engine: *mut engine_ffi::Engine,
    pub remap: Vec<(&'static CStr, CString)>,
    states: Vec<(String, String)>,
    shown: [String; 2],
    own_idle: bool,
    mouth_value: f64,
    pub bg_clock_ms: f64,
    asleep: bool,
    action_left_ms: f64,
    random_wait_ms: f64,
    pool: Vec<String>,
    action_every_ms: (f64, f64),
    pub head: [u8; 4],
    head_at: [f64; 2],
    head_dt_ms: f64,
    pub head_lead_ms: f32,
}

fn random_u32() -> u32 {
    let hasher = RandomState::new().build_hasher();
    hasher.finish() as u32
}

fn param_ptr(remap: &[(&'static CStr, CString)], id: &CStr) -> *const c_char {
    match remap.iter().find(|r| r.0 == id) {
        Some(r) => r.1.as_ptr(),
        None => id.as_ptr(),
    }
}

extern "C" fn apply_early(context: *mut c_void) {
    let behavior = unsafe { &mut *(context as *mut Behavior) };
    let mut head = [0.0f32; 2];
    let lead = behavior.head_lead_ms > 0.0 && !behavior.asleep;
    if lead {
        let ids = [param_ptr(&behavior.remap, c"ParamAngleX"), param_ptr(&behavior.remap, c"ParamAngleY")];
        unsafe { engine_ffi::engine_params_ahead(behavior.engine, behavior.head_lead_ms, ids.as_ptr(), head.as_mut_ptr(), 2) };
    }
    for (index, id) in [c"ParamAngleX", c"ParamAngleY", c"ParamAngleZ"].into_iter().enumerate() {
        let v = unsafe { engine_ffi::engine_param_get(behavior.engine, param_ptr(&behavior.remap, id)) };
        unsafe { engine_ffi::engine_param_set(behavior.engine, param_ptr(&behavior.remap, id), v * HEAD_TURN_SCALE) };
        if index < 2 && !lead {
            head[index] = v;
        }
    }
    behavior.capture_head([head[0] * HEAD_TURN_SCALE, head[1] * HEAD_TURN_SCALE]);
}

extern "C" fn apply_mouth(context: *mut c_void) {
    let behavior = unsafe { &mut *(context as *mut Behavior) };
    if behavior.mouth_value >= 0.0 {
        unsafe { engine_ffi::engine_param_set(behavior.engine, param_ptr(&behavior.remap, c"ParamMouthOpenY"), behavior.mouth_value as f32) };
    }
}

impl Behavior {
    pub fn new(engine: *mut engine_ffi::Engine, setup: &VtubeSetup, shipped: &[(String, PathBuf)], actions: &[(String, PathBuf)], idle_loop: &str) -> Behavior {
        let mut remap: Vec<(&'static CStr, CString)> = Vec::new();
        for (id, input) in VTS_INPUTS {
            if unsafe { engine_ffi::engine_has_param(engine, id.as_ptr()) } {
                continue;
            }
            let found = setup.outputs.get(input).into_iter().flatten().filter_map(|output| CString::new(output.as_str()).ok()).find(|output| unsafe { engine_ffi::engine_has_param(engine, output.as_ptr()) });
            if let Some(output) = found {
                remap.push((id, output));
            }
        }
        for (name, path) in setup.animations.iter().chain(actions.iter().filter(|(name, _)| !setup.actions.contains(name))) {
            if let (Ok(group), Ok(file)) = (CString::new(name.as_str()), CString::new(path.to_string_lossy().as_bytes())) {
                unsafe { engine_ffi::engine_motion_add(engine, group.as_ptr(), file.as_ptr()) };
            }
        }
        let mut states = setup.states.clone();
        for (name, path) in setup.state_files.iter().chain(shipped) {
            if states.iter().any(|state| &state.0 == name) {
                continue;
            }
            let (Ok(key), Ok(file)) = (CString::new(format!("file:{}", path.display())), CString::new(path.to_string_lossy().as_bytes())) else { continue };
            if unsafe { engine_ffi::engine_expression_add(engine, key.as_ptr(), file.as_ptr()) } {
                states.push((name.clone(), key.into_string().unwrap_or_default()));
            }
        }
        let picked = setup.idle_loops.iter().find(|row| row.0 == idle_loop).or(setup.idle_loops.first());
        if let Some(row) = picked {
            println!("hikari: idle loop {}", row.0);
        }
        let loop_file = picked.and_then(|row| CString::new(row.1.to_string_lossy().as_bytes()).ok());
        let own_idle = loop_file.is_some_and(|file| unsafe { engine_ffi::engine_motion_add(engine, file.as_ptr(), file.as_ptr()) && engine_ffi::engine_motion_idle(engine, file.as_ptr()) });
        unsafe { engine_ffi::engine_idle_motions_enable(engine, true) };
        Behavior {
            engine,
            remap,
            states,
            shown: [String::new(), String::new()],
            own_idle,
            mouth_value: -1.0,
            bg_clock_ms: 0.0,
            asleep: false,
            action_left_ms: 0.0,
            random_wait_ms: -1.0,
            pool: Vec::new(),
            action_every_ms: (15000.0, 15000.0),
            head: [0, 0, 0, 150],
            head_at: [0.0, PITCH_HOME],
            head_dt_ms: 0.0,
            head_lead_ms: 0.0,
        }
    }

    fn advance_wait(&mut self, dt_ms: f64) {
        self.action_left_ms -= dt_ms;
        if self.mouth_value >= 0.0 || self.asleep || self.action_left_ms > 0.0 {
            self.random_wait_ms = -1.0;
        } else if self.random_wait_ms < 0.0 {
            let (min, max) = self.action_every_ms;
            self.random_wait_ms = min + (random_u32() as f64 / (u32::MAX as f64 + 1.0) * (max - min + 1.0)).floor();
        } else {
            self.random_wait_ms -= dt_ms;
            if self.random_wait_ms <= 0.0 {
                self.random_wait_ms = -1.0;
                if let Some(name) = self.start_action() {
                    println!("hikari: idle random action={}", name);
                }
            }
        }
    }

    pub fn react(&mut self, kind: &str) -> Value {
        let Ok(group) = CString::new(kind) else { return Value::Null };
        let seconds = unsafe { engine_ffi::engine_motion_play(self.engine, group.as_ptr(), kind == "sleep") };
        if seconds < 0.0 {
            return Value::Null;
        }
        self.action_left_ms = seconds as f64 * 1000.0;
        json!({ "action": kind, "motion": kind, "expression": null })
    }

    pub fn set_state(&mut self, layer: usize, name: &str) {
        if self.shown[layer] == name {
            return;
        }
        self.shown[layer] = name.to_string();
        self.play_state(layer);
    }

    fn play_state(&self, layer: usize) {
        let key = self.states.iter().find(|state| state.0 == self.shown[layer]).map_or("", |state| state.1.as_str());
        let Ok(key) = CString::new(key) else { return };
        unsafe { engine_ffi::engine_expression(self.engine, layer as i32, key.as_ptr()) };
    }

    fn start_action(&mut self) -> Option<String> {
        if self.pool.is_empty() {
            return None;
        }
        let name = self.pool[random_u32() as usize % self.pool.len()].clone();
        self.react(&name);
        Some(name)
    }

    pub fn set_actions(&mut self, names: &[String], every_seconds: (u64, u64)) {
        let next_pool = if self.own_idle { Vec::new() } else { names.to_vec() };
        let next_interval = (every_seconds.0 as f64 * 1000.0, every_seconds.1 as f64 * 1000.0);
        if self.action_every_ms != next_interval {
            self.random_wait_ms = -1.0;
        }
        self.pool = next_pool;
        self.action_every_ms = next_interval;
    }

    pub fn sleep_begin(&mut self) -> Value {
        unsafe {
            engine_ffi::engine_breath_enable(self.engine, false);
            engine_ffi::engine_idle_motions_enable(self.engine, false);
            engine_ffi::engine_expressions_stop(self.engine);
        }
        self.asleep = true;
        self.react("sleep")
    }

    pub fn sleep_end(&mut self) {
        unsafe {
            engine_ffi::engine_breath_enable(self.engine, true);
            engine_ffi::engine_idle_motions_enable(self.engine, false);
            engine_ffi::engine_idle_motions_enable(self.engine, true);
        }
        self.asleep = false;
        self.play_state(1);
        self.play_state(0);
    }

    fn capture_head(&mut self, head: [f32; 2]) {
        let (angle_x, angle_y) = (head[0] as f64, head[1] as f64);
        let target = if self.asleep { [0.0, 0.0] } else { [(-angle_x / YAW_PER_ANGLE_X * 10.0).clamp(-800.0, 800.0), (PITCH_HOME + angle_y / PITCH_PER_ANGLE_Y * 10.0).clamp(0.0, 600.0)] };
        let blend = 1.0 - (-self.head_dt_ms / HEAD_SMOOTH_MS).exp();
        self.head_at[0] += (target[0] - self.head_at[0]) * blend;
        self.head_at[1] += (target[1] - self.head_at[1]) * blend;
        let yaw = self.head_at[0].round() as i16;
        let pitch = self.head_at[1].round() as i16;
        self.head[..2].copy_from_slice(&yaw.to_be_bytes());
        self.head[2..].copy_from_slice(&pitch.to_be_bytes());
    }

    pub fn update(&mut self, dt_ms: f32, mouth: f32) {
        let dt = dt_ms as f64;
        self.head_dt_ms = dt;
        self.mouth_value = mouth as f64;
        self.bg_clock_ms += dt;
        self.advance_wait(dt);
        let engine = self.engine;
        let context = self as *mut Behavior as *mut c_void;
        unsafe { engine_ffi::engine_update(engine, dt_ms, apply_early, apply_mouth, context) };
    }
}
