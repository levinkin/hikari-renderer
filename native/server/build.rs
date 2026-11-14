use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

fn collect_sources(dir: &Path, skip: &[&str], out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().and_then(OsStr::to_str).unwrap_or_default().to_string();
        if name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            if !skip.contains(&name.as_str()) {
                collect_sources(&path, skip, out);
            }
            continue;
        }
        let ext = path.extension().and_then(OsStr::to_str).unwrap_or_default();
        if (ext == "cpp" || ext == "mm") && !skip.contains(&name.as_str()) {
            out.push(path);
        }
    }
}

fn find_zip(repo: &Path) -> Option<PathBuf> {
    let entries = fs::read_dir(repo).ok()?;
    let zips = entries.flatten().map(|entry| entry.path()).filter(|path| {
        let name = path.file_name().and_then(OsStr::to_str).unwrap_or_default();
        name.starts_with("CubismSdkForNative-") && name.ends_with(".zip") && path.is_file()
    });
    zips.max()
}

fn unpack_sdk(zip_path: &Path, sdk_root: &Path, samples_dir: &Path, inputs: &[&str]) {
    let resources = Path::new("Samples/Resources");
    fs::remove_dir_all(sdk_root).ok();
    fs::remove_dir_all(samples_dir).ok();
    let mut archive = zip::ZipArchive::new(fs::File::open(zip_path).unwrap()).unwrap();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).unwrap();
        let name = entry.name().unwrap().replace('\\', "/");
        let raw = PathBuf::from(&name);
        if raw.is_absolute() || name.contains(':') || raw.components().any(|part| !matches!(part, Component::Normal(_) | Component::CurDir)) {
            panic!("refusing unsafe Cubism SDK zip entry {}", name);
        }
        let kind = entry.unix_mode().unwrap_or(0o100000) & 0o170000;
        if entry.is_dir() || entry.is_symlink() || (kind != 0o100000 && kind != 0) {
            continue;
        }
        let inner: PathBuf = raw.components().skip(1).collect();
        let model = inner.strip_prefix(resources).ok().filter(|rest| rest.components().count() > 1);
        let out = match model {
            Some(rest) => samples_dir.join(rest),
            None if inputs.iter().any(|prefix| inner.starts_with(prefix)) => sdk_root.join(&inner),
            None => continue,
        };
        fs::create_dir_all(out.parent().unwrap()).unwrap();
        io::copy(&mut entry, &mut fs::File::create(&out).unwrap()).unwrap();
    }
}

fn embed_metal(framework_dir: &Path, out_dir: &Path) {
    let shader_dir = framework_dir.join("Rendering/Metal/Shaders");
    let mut blend = String::new();
    for line in fs::read_to_string(shader_dir.join("BlendShaderFuncs.h")).unwrap().lines() {
        let text = match line.trim().strip_prefix("#include \"").and_then(|rest| rest.strip_suffix('"')) {
            Some(include) => fs::read_to_string(shader_dir.join(include)).unwrap(),
            None => format!("{}\n", line),
        };
        for part in text.lines() {
            blend.push_str(part);
            blend.push('\n');
        }
    }
    fs::write(out_dir.join("blend_shaders.metal"), blend).unwrap();
    println!("cargo:rerun-if-changed={}", shader_dir.display());
}

fn patch_physics(framework_dir: &Path, out_dir: &Path) -> Option<PathBuf> {
    let source_path = framework_dir.join("Physics/CubismPhysics.cpp");
    let mut source = match fs::read_to_string(&source_path) {
        Ok(source) => source,
        Err(error) => {
            println!("cargo::error=can't read {}: {}", source_path.display(), error);
            return None;
        }
    };
    for indent in ["            ", "                "] {
        let anchor = format!("{indent}    _options.Gravity\n{indent});\n");
        if source.matches(anchor.as_str()).count() != 1 {
            println!("cargo::error={} does not match the angle unwrap patch: hikari expects Cubism SDK for Native 5-r.5", source_path.display());
            return None;
        }
        let unwrapped = format!("{anchor}{indent}csmFloat32 step = outputValue - _currentRigOutputs[settingIndex].outputs[i];\n{indent}if (currentOutputs[i].GetValue == GetOutputAngle && std::fabs(step) > CubismMath::Pi)\n{indent}{{\n{indent}    csmFloat32 unwrapped = _currentRigOutputs[settingIndex].outputs[i] + std::remainder(step, 2.0f * CubismMath::Pi);\n{indent}    if (physics_drop_turns)\n{indent}    {{\n{indent}        _previousRigOutputs[settingIndex].outputs[i] += outputValue - unwrapped;\n{indent}    }}\n{indent}    else\n{indent}    {{\n{indent}        outputValue = unwrapped;\n{indent}    }}\n{indent}}}\n");
        source = source.replacen(anchor.as_str(), &unwrapped, 1);
    }
    let substep = "        _currentRemainTime -= physicsDeltaTime;\n";
    let header = "#include \"CubismPhysics.hpp\"\n";
    if source.matches(substep).count() != 1 || source.matches(header).count() != 1 {
        println!("cargo::error={} does not match the turn drop patch: hikari expects Cubism SDK for Native 5-r.5", source_path.display());
        return None;
    }
    source = source.replacen(substep, &format!("        physics_drop_turns = false;\n{substep}"), 1).replacen(header, &format!("{header}thread_local bool physics_drop_turns = false;\n"), 1);
    let patched = out_dir.join("CubismPhysics.cpp");
    fs::write(&patched, source).unwrap();
    Some(patched)
}

fn patch_expression(framework_dir: &Path, out_dir: &Path) -> Option<PathBuf> {
    let source_path = framework_dir.join("Motion/CubismExpressionMotion.cpp");
    let source = match fs::read_to_string(&source_path) {
        Ok(source) => source,
        Err(error) => {
            println!("cargo::error=can't read {}: {}", source_path.display(), error);
            return None;
        }
    };
    let anchor = "const csmFloat32 currentParameterValue = expressionParameterValue.OverwriteValue =\n";
    if source.matches(anchor).count() != 1 {
        println!("cargo::error={} does not match the overwrite fade patch: hikari expects Cubism SDK for Native 5-r.5", source_path.display());
        return None;
    }
    let patched = out_dir.join("CubismExpressionMotion.cpp");
    if let Err(error) = fs::write(&patched, source.replacen(anchor, "const csmFloat32 currentParameterValue =\n", 1)) {
        println!("cargo::error=can't write {}: {}", patched.display(), error);
        return None;
    }
    Some(patched)
}

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let repo = manifest_dir.parent().unwrap().parent().unwrap().to_path_buf();
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap();
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    let core_path = match (target_os.as_str(), target_arch.as_str()) {
        ("windows", _) => "Core/dll/windows/x86_64/Live2DCubismCore.dll",
        ("macos", _) => "Core/dll/macos/libLive2DCubismCore.dylib",
        ("ios", _) => "Core/lib/ios",
        (_, "aarch64") => "Core/dll/experimental/linux/arm64/libLive2DCubismCore.so",
        _ => "Core/dll/linux/x86_64/libLive2DCubismCore.so",
    };
    let engine_dir = manifest_dir.join("../engine");
    let mut sdk_root = repo.join("sdk");
    let profile_dir = out_dir.ancestors().nth(3).unwrap();
    let inputs = ["Framework/src", "Core/include", core_path, "Samples/Vulkan/thirdParty/stb"];
    if sdk_root.join("Framework/src").is_dir() {
        for input in inputs {
            println!("cargo:rerun-if-changed={}", sdk_root.join(input).display());
        }
    } else if let Some(zip_path) = find_zip(&repo) {
        println!("cargo:rerun-if-changed={}", zip_path.display());
        sdk_root = out_dir.join("sdk");
        let meta = fs::metadata(&zip_path).unwrap();
        let stamp = format!("{}\n{}\n{:?}", zip_path.display(), meta.len(), meta.modified().unwrap());
        let stamp_path = out_dir.join("sdk.stamp");
        let samples_dir = profile_dir.parent().unwrap().join("samples");
        let extracted = inputs.iter().all(|input| sdk_root.join(input).exists()) && samples_dir.is_dir();
        if !extracted || fs::read_to_string(&stamp_path).ok().as_deref() != Some(stamp.as_str()) {
            unpack_sdk(&zip_path, &sdk_root, &samples_dir, &inputs);
            fs::write(&stamp_path, stamp).unwrap();
        }
    }
    let framework_dir = sdk_root.join("Framework/src");
    if !framework_dir.is_dir() {
        println!("cargo::error=no Cubism SDK in {}: download the Cubism SDK for Native zip from https://www.live2d.com/en/sdk/download/native/ and drop it in {}", repo.display(), repo.display());
        return;
    }
    if target_os != "ios" {
        fs::copy(sdk_root.join(core_path), profile_dir.join(Path::new(core_path).file_name().unwrap())).unwrap();
    }
    let sdk_define = format!("\"{}\"", sdk_root.display().to_string().replace('\\', "/"));
    let core_include = sdk_root.join("Core/include");
    let Some(physics_source) = patch_physics(&framework_dir, &out_dir) else {
        return;
    };
    let Some(expression_source) = patch_expression(&framework_dir, &out_dir) else {
        return;
    };
    if target_os == "macos" || target_os == "ios" {
        let mut framework_sources = vec![physics_source, expression_source];
        collect_sources(&framework_dir, &["D3D9", "D3D11", "OpenGL", "Vulkan", "Metal", "build", "CubismPhysics.cpp", "CubismExpressionMotion.cpp"], &mut framework_sources);
        embed_metal(&framework_dir, &out_dir);
        framework_sources.push(engine_dir.join("core_loader.cpp"));
        cc::Build::new()
            .cpp(true)
            .std("c++17")
            .flag("-fobjc-arc")
            .flag("-Wno-c23-extensions")
            .flag(format!("--embed-dir={}", framework_dir.display()))
            .flag(format!("--embed-dir={}", out_dir.display()))
            .include(&engine_dir)
            .include(engine_dir.join("syphon"))
            .include(&framework_dir)
            .include(&core_include)
            .warnings(false)
            .file(engine_dir.join("engine.mm"))
            .file(engine_dir.join("renderer_metal.mm"))
            .compile("hikari_engine");
        cc::Build::new()
            .cpp(true)
            .std("c++17")
            .flag("-fno-objc-arc")
            .define("HIKARI_SDK", sdk_define.as_str())
            .include(&framework_dir)
            .include(framework_dir.join("Physics")).include(framework_dir.join("Motion"))
            .include(&core_include)
            .warnings(false)
            .files(&framework_sources)
            .compile("cubism_framework");
        if target_os == "ios" {
            let arch = if target_arch == "aarch64" { "arm64" } else { target_arch.as_str() };
            let core_lib = if env::var("CARGO_CFG_TARGET_ABI").is_ok_and(|abi| abi == "sim") || target_arch == "x86_64" {
                sdk_root.join("Core/lib/ios").join(format!("Release-iphonesimulator-{}", arch))
            } else {
                sdk_root.join("Core/lib/ios/Release-iphoneos")
            };
            println!("cargo:rustc-link-search=native={}", core_lib.display());
            println!("cargo:rustc-link-lib=static=Live2DCubismCore");
        }
        for framework in ["Metal", "Foundation", "ImageIO", "CoreGraphics"] {
            println!("cargo:rustc-link-lib=framework={}", framework);
        }
        if target_os == "macos" {
            let syphon_dir = engine_dir.join("syphon");
            let syphon_sources: Vec<PathBuf> = fs::read_dir(&syphon_dir).unwrap().map(|entry| entry.unwrap().path()).filter(|path| matches!(path.extension().and_then(OsStr::to_str), Some("m" | "c"))).collect();
            cc::Build::new()
                .flag("-fobjc-arc")
                .flag("-Wno-deprecated-declarations")
                .flag("-include")
                .flag(syphon_dir.join("Syphon_Prefix.pch").to_str().unwrap())
                .include(&syphon_dir)
                .warnings(false)
                .files(&syphon_sources)
                .compile("syphon");
            for framework in ["AppKit", "QuartzCore", "IOSurface"] {
                println!("cargo:rustc-link-lib=framework={}", framework);
            }
        }
    } else {
        let mut framework_sources = vec![framework_dir.join("Rendering/Vulkan/CubismClass_Vulkan.cpp"), physics_source, expression_source];
        collect_sources(&framework_dir, &["D3D9", "D3D11", "OpenGL", "Metal", "Vulkan", "CubismPhysics.cpp", "CubismExpressionMotion.cpp"], &mut framework_sources);
        framework_sources.push(engine_dir.join("core_loader.cpp"));
        let mut engine = cc::Build::new();
        let mut framework = cc::Build::new();
        for build in [&mut engine, &mut framework] {
            build.cpp(true).std("c++17").include(&framework_dir).include(framework_dir.join("Rendering/Vulkan")).include(&core_include).include(engine_dir.join("vulkan/include")).warnings(false);
            if target_os == "windows" {
                build.flag("/utf-8").define("NOMINMAX", None);
            }
        }
        engine.include(&engine_dir).include(sdk_root.join("Samples/Vulkan/thirdParty/stb")).file(engine_dir.join("engine_vk.cpp")).file(engine_dir.join("renderer_vk.cpp")).file(engine_dir.join("kqueue_compat.cpp")).file(engine_dir.join(if target_os == "windows" { "window_win32.cpp" } else { "window_x11.cpp" })).compile("hikari_engine");
        framework.define("HIKARI_SDK", sdk_define.as_str()).include(framework_dir.join("Physics")).include(framework_dir.join("Motion")).files(&framework_sources).compile("cubism_framework");
        if target_os == "windows" {
            let target = env::var("TARGET").unwrap();
            let lib_exe = cc::windows_registry::find_tool(&target, "lib.exe").expect("lib.exe from visual studio build tools");
            let status = lib_exe
                .to_command()
                .arg(format!("/def:{}", engine_dir.join("vulkan/vulkan-1.def").display()))
                .arg(if target_arch == "aarch64" { "/machine:arm64" } else { "/machine:x64" })
                .arg(format!("/out:{}", out_dir.join("vulkan-1.lib").display()))
                .arg("/nologo")
                .status()
                .unwrap();
            assert!(status.success(), "lib.exe failed to make vulkan-1.lib");
            println!("cargo:rustc-link-search=native={}", out_dir.display());
            println!("cargo:rustc-link-lib=dylib=vulkan-1");
            println!("cargo:rustc-link-lib=dylib=user32");
            println!("cargo:rustc-link-lib=dylib=d3d11");
            println!("cargo:rustc-link-lib=dylib=dxgi");
        } else {
            println!("cargo:rustc-link-lib=dylib:+verbatim=libvulkan.so.1");
            println!("cargo:rustc-link-lib=dylib=X11");
        }
    }
    println!("cargo:rerun-if-changed={}", engine_dir.display());
    println!("cargo:rerun-if-changed=build.rs");
}
