use std::env::consts::{EXE_SUFFIX, OS};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{exit, Command, Stdio};

const URL: &str = "https://www.live2d.com/en/sdk/download/native/";
const XCODE: &str = "xcode command line tools";
const VISUAL_STUDIO: &str = "visual studio build tools with c++";
const RUST: &str = "rust";
const LINUX_TOOLS: [&str; 5] = ["c++ compiler", "x11 headers", "vulkan loader", "curl", RUST];
const LINUX_PACKAGES: [(&str, [&str; 4]); 3] = [
    ("apt-get", ["build-essential", "libx11-dev", "libvulkan1", "curl"]),
    ("dnf", ["gcc-c++", "libX11-devel", "vulkan-loader", "curl"]),
    ("pacman", ["gcc", "libx11", "vulkan-icd-loader", "curl"]),
];
const RUSTUP: &str = "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y";

struct Launch {
    root: PathBuf,
    args: Vec<OsString>,
    pause: bool,
}

fn quit(launch: &Launch, code: i32) -> ! {
    if launch.pause && code != 0 {
        print!("hikari: press enter to close ");
        io::stdout().flush().ok();
        io::stdin().lock().read_line(&mut String::new()).ok();
    }
    exit(code);
}

fn ask(question: &str, default: bool) -> Option<bool> {
    print!("hikari: {} ", question);
    io::stdout().flush().ok();
    let mut answer = String::new();
    match io::stdin().lock().read_line(&mut answer) {
        Ok(0) | Err(_) => None,
        Ok(_) if answer.trim().is_empty() => Some(default),
        Ok(_) => Some(matches!(answer.trim(), "y" | "Y")),
    }
}

fn succeeds(program: impl AsRef<OsStr>, args: &[&str]) -> bool {
    let status = Command::new(program).args(args).stdout(Stdio::null()).stderr(Stdio::null()).status();
    status.is_ok_and(|status| status.success())
}

fn find_cargo() -> Option<PathBuf> {
    let home = std::env::var_os(if OS == "windows" { "USERPROFILE" } else { "HOME" }).unwrap_or_default();
    let installed = PathBuf::from(home).join(".cargo").join("bin").join(format!("cargo{}", EXE_SUFFIX));
    [PathBuf::from("cargo"), installed].into_iter().find(|cargo| succeeds(cargo, &["--version"]))
}

fn has_visual_studio() -> bool {
    let program_files = PathBuf::from(std::env::var_os("ProgramFiles(x86)").unwrap_or_default());
    let vswhere = program_files.join("Microsoft Visual Studio").join("Installer").join("vswhere.exe");
    let found = Command::new(vswhere)
        .args(["-products", "*", "-requires", "Microsoft.VisualStudio.Component.VC.Tools.x86.x64", "-property", "installationPath"])
        .stderr(Stdio::null())
        .output();
    found.is_ok_and(|output| output.status.success() && !output.stdout.trim_ascii().is_empty())
}

fn has_tool(tool: &str, has_cargo: bool) -> bool {
    match tool {
        XCODE => succeeds("xcode-select", &["-p"]),
        VISUAL_STUDIO => has_visual_studio(),
        "c++ compiler" => succeeds("c++", &["--version"]),
        "x11 headers" => Path::new("/usr/include/X11/Xlib.h").is_file(),
        "vulkan loader" => {
            let cache = Command::new("/sbin/ldconfig").arg("-p").stderr(Stdio::null()).output();
            cache.is_ok_and(|output| String::from_utf8_lossy(&output.stdout).lines().any(|line| line.trim_start().starts_with("libvulkan.so.1 ")))
        }
        "curl" => succeeds("curl", &["--version"]),
        RUST => has_cargo,
        _ => false,
    }
}

fn missing_tools(has_cargo: bool) -> Vec<&'static str> {
    let tools: &[&'static str] = match OS {
        "macos" => &[XCODE, RUST],
        "windows" => &[VISUAL_STUDIO, RUST],
        _ => &LINUX_TOOLS,
    };
    tools.iter().copied().filter(|tool| !has_tool(tool, has_cargo)).collect()
}

fn run_step(launch: &Launch, command: &mut Command) {
    if !command.status().is_ok_and(|status| status.success()) {
        println!("hikari: {:?} failed", command);
        quit(launch, 1);
    }
}

fn privileged(program: &str) -> Command {
    if succeeds("sudo", &["--version"]) {
        let mut command = Command::new("sudo");
        command.arg(program);
        command
    } else {
        Command::new(program)
    }
}

fn install_linux(launch: &Launch, missing: &[&str]) {
    let packages = LINUX_PACKAGES.iter().find(|(manager, _)| succeeds(manager, &["--version"]));
    let Some((manager, names)) = packages else {
        println!("hikari: no apt-get, dnf or pacman here, install these yourself: {}", missing.join(", "));
        quit(launch, 1);
    };
    let wanted: Vec<&str> = LINUX_TOOLS.iter().zip(names).filter(|(tool, _)| missing.contains(tool)).map(|(_, name)| *name).collect();
    if !wanted.is_empty() {
        match *manager {
            "apt-get" => {
                run_step(launch, privileged(manager).arg("update"));
                run_step(launch, privileged(manager).args(["install", "-y"]).args(&wanted));
            }
            "dnf" => run_step(launch, privileged(manager).args(["install", "-y"]).args(&wanted)),
            _ => run_step(launch, privileged(manager).args(["-S", "--needed", "--noconfirm"]).args(&wanted)),
        }
    }
    if missing.contains(&RUST) {
        run_step(launch, Command::new("sh").args(["-c", RUSTUP]));
    }
}

fn install_windows(launch: &Launch, missing: &[&str]) {
    if !succeeds("winget", &["--version"]) {
        println!("hikari: no winget here, install https://visualstudio.microsoft.com/visual-cpp-build-tools/ (desktop development with c++) and https://rustup.rs yourself, then run this again");
        quit(launch, 1);
    }
    let agree = ["--accept-package-agreements", "--accept-source-agreements"];
    if missing.contains(&VISUAL_STUDIO) {
        let workload = "--quiet --wait --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended";
        run_step(launch, Command::new("winget").args(["install", "--id", "Microsoft.VisualStudio.2022.BuildTools", "--override", workload]).args(agree));
    }
    if missing.contains(&RUST) {
        run_step(launch, Command::new("winget").args(["install", "--id", "Rustlang.Rustup"]).args(agree));
    }
}

fn install_mac(launch: &Launch, missing: &[&str]) {
    if missing.contains(&XCODE) {
        run_step(launch, Command::new("xcode-select").arg("--install"));
        if ask("press enter once the command line tools installer finishes", true).is_none() || !has_tool(XCODE, false) {
            println!("hikari: xcode command line tools still missing, finish their installer and run this again");
            quit(launch, 1);
        }
    }
    if missing.contains(&RUST) {
        run_step(launch, Command::new("sh").args(["-c", RUSTUP]));
    }
}

fn check_tools(launch: &Launch) -> PathBuf {
    let mut cargo = find_cargo();
    let missing = missing_tools(cargo.is_some());
    if !missing.is_empty() {
        println!("hikari: building needs {}", missing.join(", "));
        if ask("install them now? [Y/n]", true) != Some(true) {
            quit(launch, 1);
        }
        match OS {
            "macos" => install_mac(launch, &missing),
            "windows" => install_windows(launch, &missing),
            _ => install_linux(launch, &missing),
        }
        cargo = find_cargo();
        let still = missing_tools(cargo.is_some());
        if !still.is_empty() {
            println!("hikari: still missing {} after installing", still.join(", "));
            quit(launch, 1);
        }
    }
    match cargo {
        Some(cargo) => cargo,
        None => quit(launch, 1),
    }
}

fn open_page() {
    let mut command = match OS {
        "macos" => Command::new("open"),
        "windows" => {
            let mut command = Command::new("cmd");
            command.args(["/c", "start", ""]);
            command
        }
        _ => Command::new("xdg-open"),
    };
    command.arg(URL).stdout(Stdio::null()).stderr(Stdio::null());
    command.status().ok();
}

fn build(launch: &Launch, cargo: &Path, target: &Path) {
    let manifest = launch.root.join("native").join("server").join("Cargo.toml");
    let status = Command::new(cargo)
        .args(["build", "--release", "--quiet", "--manifest-path"])
        .arg(manifest)
        .arg("--target-dir")
        .arg(target)
        .current_dir(&launch.root)
        .status();
    match status {
        Ok(status) if status.success() => {}
        Ok(status) => quit(launch, status.code().unwrap_or(1)),
        Err(error) => {
            eprintln!("hikari: cargo: {}", error);
            quit(launch, 1);
        }
    }
}

fn has_sdk(root: &Path) -> bool {
    let zipped = fs::read_dir(root).is_ok_and(|entries| {
        entries.flatten().any(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            name.starts_with("CubismSdkForNative-") && name.ends_with(".zip") && entry.path().is_file()
        })
    });
    root.join("sdk").join("Framework").join("src").is_dir() || zipped
}

fn copy_dir(from: &Path, to: &Path) -> io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &to.join(entry.file_name()))?;
        } else {
            fs::copy(entry.path(), to.join(entry.file_name()))?;
        }
    }
    Ok(())
}

fn folders(dir: &Path) -> Vec<OsString> {
    let mut names: Vec<OsString> = fs::read_dir(dir)
        .map(|entries| entries.flatten().filter(|entry| entry.path().is_dir()).map(|entry| entry.file_name()).collect())
        .unwrap_or_default();
    names.sort();
    names
}

fn offer_samples(launch: &Launch, target: &Path) {
    let built_samples = target.join("samples");
    let samples = if built_samples.is_dir() { built_samples } else { launch.root.join("sdk").join("Samples").join("Resources") };
    let models = launch.root.join("models");
    let names = folders(&samples);
    if !names.is_empty() {
        let listed: Vec<String> = names.iter().map(|name| name.to_string_lossy().into_owned()).collect();
        let answer = ask(&format!("copy live2d's sample models ({}) into models/? [Y/n]", listed.join(", ")), true);
        if answer.is_none() {
            quit(launch, 1);
        }
        if answer == Some(true) {
            for name in names.iter().filter(|name| !models.join(name).exists()) {
                if let Err(error) = copy_dir(&samples.join(name), &models.join(name)) {
                    eprintln!("hikari: copying {}: {}", name.to_string_lossy(), error);
                    fs::remove_dir_all(models.join(name)).ok();
                    quit(launch, 1);
                }
            }
        }
    }
    if launch.pause && folders(&models).is_empty() {
        println!("hikari: models/ is empty, drop a model folder into models/ and run this again");
        quit(launch, 1);
    }
}

fn clean_up(launch: &Launch, built: &Path, installed: &Path, target: &Path) {
    let core = match OS {
        "windows" => "Live2DCubismCore.dll",
        "macos" => "libLive2DCubismCore.dylib",
        _ => "libLive2DCubismCore.so",
    };
    let built_core = built.with_file_name(core);
    let root_core = launch.root.join(core);
    if let Err(error) = fs::rename(&built_core, &root_core) {
        eprintln!("hikari: clean up: {}: {}", built_core.display(), error);
        quit(launch, 1);
    }
    if let Err(error) = fs::rename(built, installed) {
        fs::rename(&root_core, &built_core).ok();
        eprintln!("hikari: clean up: {}: {}", built.display(), error);
        quit(launch, 1);
    }
    if let Err(error) = fs::remove_dir_all(target) {
        eprintln!("hikari: clean up: {}", error);
        quit(launch, 1);
    }
}

fn run(launch: &Launch, program: &Path) -> ! {
    let mut command = Command::new(program);
    command.args(&launch.args).current_dir(&launch.root);
    #[cfg(unix)]
    if !launch.pause {
        use std::os::unix::process::CommandExt;
        let error = command.exec();
        eprintln!("hikari: {}: {}", program.display(), error);
        exit(1);
    }
    match command.status() {
        Ok(status) => quit(launch, status.code().unwrap_or(1)),
        Err(error) => {
            eprintln!("hikari: {}: {}", program.display(), error);
            quit(launch, 1);
        }
    }
}

fn main() {
    let mut given: Vec<OsString> = std::env::args_os().skip(1).collect();
    let rebuild = given.iter().any(|arg| arg == "--build");
    given.retain(|arg| arg != "--build");
    let pause = given.is_empty();
    let headless = given.iter().any(|arg| arg == "--headless");
    given.retain(|arg| arg != "--headless");
    let args = if headless || given.iter().any(|arg| arg == "--window") { given } else { [vec![OsString::from("--window")], given].concat() };
    let root = std::env::current_exe().ok().and_then(|exe| exe.parent().map(Path::to_path_buf));
    let launch = Launch { root: root.unwrap_or_default(), args, pause };
    let name = format!("hikari{}", EXE_SUFFIX);
    let installed = launch.root.join(format!("hikari-renderer{}", EXE_SUFFIX));
    if installed.is_file() && !rebuild {
        run(&launch, &installed);
    }
    while !has_sdk(&launch.root) {
        println!("hikari: no Cubism SDK yet. hikari needs live2d's Cubism SDK for Native, the native one, not web, unity or java.");
        println!("hikari: download its zip from {} and accept their license, then drop the zip next to {} as it is.", URL, name);
        match ask("open the download page? [y/n]", false) {
            None => quit(&launch, 1),
            Some(true) => open_page(),
            Some(false) => {}
        }
        if ask("is the zip in place now? [y/n]", false) != Some(true) {
            quit(&launch, 1);
        }
    }
    let cargo = check_tools(&launch);
    let target = launch.root.join("native").join("server").join("target");
    let built = target.join("release").join(&name);
    if installed.is_file() {
        println!("hikari: rebuilding, this takes about a minute");
        build(&launch, &cargo, &target);
        clean_up(&launch, &built, &installed, &target);
        run(&launch, &installed);
    }
    if !built.is_file() {
        println!("hikari: building on first start, this takes about a minute");
        build(&launch, &cargo, &target);
        offer_samples(&launch, &target);
        match ask("built. clean up the build files? hikari keeps working [y/N]", false) {
            None => quit(&launch, 1),
            Some(true) => {
                clean_up(&launch, &built, &installed, &target);
                run(&launch, &installed);
            }
            Some(false) => run(&launch, &built),
        }
    }
    build(&launch, &cargo, &target);
    run(&launch, &built);
}
