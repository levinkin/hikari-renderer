use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

#[derive(Default)]
pub struct VtubeSetup {
    pub outputs: HashMap<String, Vec<String>>,
    pub expressions: Vec<String>,
    pub active_expressions: Vec<String>,
    pub physics: Option<(f32, f32)>,
    pub physics_scales: Vec<(String, f32, f32)>,
    pub framing: Option<[f32; 4]>,
    pub idle_loops: Vec<(String, PathBuf)>,
    pub animations: Vec<(String, PathBuf)>,
    pub states: Vec<(String, String)>,
    pub state_files: Vec<(String, PathBuf)>,
    pub actions: Vec<String>,
}

const READ_CAP: u64 = 16 << 20;
const WALK_DEPTH: usize = 8;
const WALK_ENTRIES: usize = 10000;
const VTS_ZOOM: f64 = 0.4575;
const VTS_HALF_WIDTH: f64 = 160.0;
const VTS_HALF_HEIGHT: f64 = 120.0;
const VTS_PIVOT_X: f64 = 0.75;
const VTS_PIVOT_Y: f64 = 1.0836;

fn file_stem(file: &str) -> String {
    let base = Path::new(file).file_name().and_then(|name| name.to_str()).unwrap_or(file);
    base.strip_suffix(".exp3.json").unwrap_or(base).to_string()
}

fn read_capped(path: &Path) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    fs::File::open(path)?.take(READ_CAP + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > READ_CAP {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("{} is over 16 MB", path.display())));
    }
    Ok(bytes)
}

fn find_file(model3_path: &Path) -> Option<Vec<u8>> {
    let dir = model3_path.parent()?;
    let model_name = model3_path.file_name()?.to_str()?.strip_suffix(".model3.json")?;
    read_capped(&dir.join(format!("{}.vtube.json", model_name))).ok()
}

fn find_motion(dir: &Path, file: &str) -> Option<PathBuf> {
    let direct = dir.join(file);
    if direct.is_file() {
        return Some(direct);
    }
    let name = Path::new(file).file_name()?;
    let mut budget = WALK_ENTRIES;
    search_motion(dir, name, WALK_DEPTH, &mut budget)
}

fn search_motion(dir: &Path, name: &std::ffi::OsStr, depth: usize, budget: &mut usize) -> Option<PathBuf> {
    for entry in fs::read_dir(dir).ok()?.flatten() {
        if *budget == 0 {
            return None;
        }
        *budget -= 1;
        let path = entry.path();
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            if depth > 0 {
                if let Some(found) = search_motion(&path, name, depth - 1, budget) {
                    return Some(found);
                }
            }
        } else if path.file_name() == Some(name) {
            return Some(path);
        }
    }
    None
}

fn framing(saved: &Value) -> Option<[f32; 4]> {
    let scale = saved["Scale"]["x"].as_f64()?;
    let (x, y) = (saved["Position"]["x"].as_f64()?, saved["Position"]["y"].as_f64()?);
    let (z, w) = (saved["Rotation"]["z"].as_f64().unwrap_or(0.0), saved["Rotation"]["w"].as_f64().unwrap_or(1.0));
    let mut th = 2.0 * z.atan2(w).to_degrees();
    if th <= -180.0 {
        th += 360.0;
    } else if th > 180.0 {
        th -= 360.0;
    }
    let zoom = VTS_ZOOM * scale;
    let lift = VTS_PIVOT_Y * zoom;
    let view = [zoom, x / VTS_HALF_WIDTH - VTS_PIVOT_X * lift * th.to_radians().sin(), y / VTS_HALF_HEIGHT + lift * th.to_radians().cos(), th].map(|v| v as f32);
    (scale > 0.0 && view.iter().all(|v| v.is_finite())).then_some(view)
}

fn group_multipliers(list: &Value) -> HashMap<String, f32> {
    let mut out = HashMap::new();
    for row in list.as_array().into_iter().flatten() {
        let (Some(id), Some(value)) = (row["ID"].as_str(), row["Value"].as_f64()) else { continue };
        if value.is_finite() {
            out.insert(id.to_string(), (value as f32).clamp(0.0, 2.0));
        }
    }
    out
}

fn physics_scales(dir: &Path, model: &Value, strengths: &HashMap<String, f32>, winds: &HashMap<String, f32>) -> Vec<(String, f32, f32)> {
    let mut out = Vec::new();
    let Some(file) = model["FileReferences"]["Physics"].as_str() else { return out };
    let Some(physics) = read_capped(&dir.join(file)).ok().and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok()) else { return out };
    for group in physics["PhysicsSettings"].as_array().into_iter().flatten() {
        let Some(id) = group["Id"].as_str() else { continue };
        let strength = strengths.get(id).copied().unwrap_or(1.0);
        let wind = winds.get(id).copied().unwrap_or(1.0);
        if strength == 1.0 && wind == 1.0 {
            continue;
        }
        for output in group["Output"].as_array().into_iter().flatten() {
            if let Some(param) = output["Destination"]["Id"].as_str() {
                out.push((param.to_string(), strength, wind));
            }
        }
    }
    out
}

fn motion_stem(path: &Path) -> String {
    let base = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    base.strip_suffix(".motion3.json").unwrap_or(&base).to_string()
}

fn add_state(states: &mut Vec<(String, String)>, name: &str, key: &str) {
    if !name.is_empty() && !states.iter().any(|state| state.0 == name) {
        states.push((name.to_string(), key.to_string()));
    }
}

fn walk_states(dir: &Path, depth: usize, budget: &mut usize, out: &mut Vec<(String, PathBuf)>) {
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        if *budget == 0 {
            return;
        }
        *budget -= 1;
        let path = entry.path();
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            if depth < WALK_DEPTH {
                walk_states(&path, depth + 1, budget, out);
            }
        } else if let Some(stem) = path.file_name().and_then(|name| name.to_str()).and_then(|name| name.strip_suffix(".exp3.json")) {
            if fs::metadata(&path).is_ok_and(|meta| meta.len() > READ_CAP) {
                eprintln!("hikari: skipping {} (over 16 MB)", path.display());
                continue;
            }
            out.push((stem.to_string(), path.clone()));
        }
    }
}

pub fn shipped_states(dir: &Path) -> Vec<(String, PathBuf)> {
    let mut out: Vec<(String, PathBuf)> = Vec::new();
    let mut budget = WALK_ENTRIES;
    walk_states(dir, 0, &mut budget, &mut out);
    out.sort();
    out
}

pub fn defaults(path: &Path) -> Result<(Vec<(String, PathBuf)>, Vec<(String, PathBuf)>), String> {
    let bytes = read_capped(path).map_err(|error| error.to_string())?;
    let model: Value = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    let dir = path.parent().unwrap_or(Path::new("."));
    let refs = model.get("FileReferences").and_then(Value::as_object).ok_or("FileReferences must be an object")?;
    let mut states = Vec::new();
    for row in refs.get("Expressions").map_or(Some(&Vec::new()), Value::as_array).ok_or("Expressions must be an array")? {
        let file = row.get("File").and_then(Value::as_str).ok_or("each Expressions entry must have a string File")?;
        let name = match row.get("Name") {
            None => file_stem(file),
            Some(name) => name.as_str().ok_or("Expressions Name must be a string")?.to_string(),
        };
        states.push((name, dir.join(file)));
    }
    let mut actions = Vec::new();
    for (name, rows) in refs.get("Motions").map_or(Some(&serde_json::Map::new()), Value::as_object).ok_or("Motions must be an object")? {
        for row in rows.as_array().ok_or(format!("Motions {} must be an array", name))? {
            let file = row.get("File").and_then(Value::as_str).ok_or(format!("each Motions {} entry must have a string File", name))?;
            actions.push((name.clone(), dir.join(file)));
        }
    }
    Ok((states, actions))
}

pub fn load(model3_path: &Path) -> VtubeSetup {
    let mut setup = VtubeSetup::default();
    let model: Value = read_capped(model3_path).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default();
    let dir = model3_path.parent().unwrap_or(Path::new("."));
    for row in model["FileReferences"]["Expressions"].as_array().into_iter().flatten() {
        let Some(file) = row["File"].as_str() else { continue };
        let key = file_stem(file);
        add_state(&mut setup.states, row["Name"].as_str().unwrap_or(""), &key);
        add_state(&mut setup.states, &key, &key);
    }
    setup.state_files = shipped_states(dir).into_iter().filter(|row| !setup.states.iter().any(|state| state.1 == row.0)).collect();
    setup.actions = model["FileReferences"]["Motions"].as_object().into_iter().flat_map(|groups| groups.keys().filter(|group| *group != "Idle").cloned()).collect();
    let idle_files: Vec<PathBuf> = model["FileReferences"]["Motions"]["Idle"].as_array().into_iter().flatten().filter_map(|row| row["File"].as_str()).map(|file| dir.join(file)).collect();
    let Some(bytes) = find_file(model3_path) else {
        setup.idle_loops = idle_files.into_iter().map(|path| (motion_stem(&path), path)).collect();
        return setup;
    };
    let Ok(root) = serde_json::from_slice::<Value>(&bytes) else {
        eprintln!("hikari: unreadable vtube.json next to {}", model3_path.display());
        return setup;
    };
    for row in root["ParameterSettings"].as_array().into_iter().flatten() {
        let (Some(input), Some(output)) = (row["Input"].as_str(), row["OutputLive2D"].as_str()) else { continue };
        if !input.is_empty() && !output.is_empty() {
            setup.outputs.entry(input.to_string()).or_default().push(output.to_string());
        }
    }
    if let Some(path) = root["FileReferences"]["IdleAnimation"].as_str().filter(|file| !file.is_empty()).and_then(|file| find_motion(dir, file)) {
        setup.idle_loops.push((motion_stem(&path), path));
    }
    for path in idle_files {
        let stem = motion_stem(&path);
        if !setup.idle_loops.iter().any(|row| row.0 == stem) {
            setup.idle_loops.push((stem, path));
        }
    }
    for hotkey in root["Hotkeys"].as_array().into_iter().flatten() {
        let name = hotkey["Name"].as_str().unwrap_or("").trim();
        match (hotkey["Action"].as_str(), hotkey["File"].as_str().filter(|file| !file.is_empty())) {
            (Some("TriggerAnimation"), Some(file)) => {
                let label = if name.is_empty() { file.strip_suffix(".motion3.json").unwrap_or(file) } else { name };
                if model["FileReferences"]["Motions"].get(label).is_some() {
                    continue;
                }
                if let Some(path) = find_motion(dir, file) {
                    setup.actions.push(label.to_string());
                    setup.animations.push((label.to_string(), path));
                }
            }
            (Some("ToggleExpression"), _) => {
                let Some(file) = hotkey["File"].as_str() else { continue };
                add_state(&mut setup.states, name, &file_stem(file));
                setup.expressions.push(file_stem(file));
            }
            _ => {}
        }
    }
    for active in root["SavedActiveExpressions"].as_array().into_iter().flatten() {
        if let Some(file) = active.as_str().or(active["File"].as_str()) {
            setup.active_expressions.push(file_stem(file));
        }
    }
    setup.framing = framing(&root["SavedModelPosition"]);
    let physics = &root["PhysicsSettings"];
    if physics["Use"].as_bool() == Some(true) {
        let strength = physics["PhysicsStrength"].as_f64().map(|v| v as f32).filter(|v| v.is_finite()).unwrap_or(50.0);
        let wind = physics["WindStrength"].as_f64().map(|v| v as f32).filter(|v| v.is_finite()).unwrap_or(0.0);
        setup.physics = Some((strength, wind));
        let custom = &root["PhysicsCustomizationSettings"];
        let strengths = group_multipliers(&custom["PhysicsMultipliersPerPhysicsGroup"]);
        let winds = group_multipliers(&custom["WindMultipliersPerPhysicsGroup"]);
        if !strengths.is_empty() || !winds.is_empty() {
            setup.physics_scales = physics_scales(dir, &model, &strengths, &winds);
        }
    }
    setup
}
