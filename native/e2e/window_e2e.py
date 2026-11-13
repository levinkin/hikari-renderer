#!/usr/bin/env python3
import argparse
import json
import os
import re
import shutil
import socket
import struct
import subprocess
import sys
import time

import vts_e2e

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
SAMPLES = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "..", "sdk", "Samples", "Resources"))
LIVE_PORT = 8793
PORTS = range(9322, 9326)
MODEL = "hiyori"
SOURCE_MODEL = "Hiyori"
SIZE = (960, 720)
RESIZED = (1600, 900)
TITLE_BAR = 40
MIN_FPS = 30
FPS_BANDS = {60: (55, 62), 30: (27, 32)}
LIT_FRACTION = 0.05
LIT_DELTA = 24
START_SECONDS = 120
WAIT_SECONDS = 15
EXIT_SECONDS = 5
TOOL_SECONDS = 60
WINDOW_LINE = re.compile(r"hikari: window size=(\d+)x(\d+) fps=(\d+)")
REACT_LINE = re.compile(r"hikari: action (\S+) motion=(\S+)")
MOUTH_LINE = re.compile(r"hikari: window mouth frames=(\d+) open_max=(\S+)")
SLEEP_LINE = re.compile(r"hikari: asleep\n(?:.*\n)*?hikari: wake")
REACT_KIND = "yawn"
OTHER_MODEL = "winmao%d" % os.getpid()
KEY_CHECKS = (
    ("9_key_q_turn", 12, "", "hikari: view model=%s "),
    ("9_key_e_turn", 14, "", "hikari: view model=%s "),
    ("9_key_space_reset", 49, "", "hikari: view model=%s "),
    ("9_key_ctrl_s_save", 1, " using {control down}", "hikari: saved view for %s"),
)
MOUTH_OPEN = 0.2
SPEECH_SLACK = 1.0
SLEEP_SECONDS = 1
WINDOW_INFO = """import AppKit
let info = CGWindowListCopyWindowInfo([.optionOnScreenOnly], kCGNullWindowID) as! [[String: Any]]
let windows = info.filter { ($0[kCGWindowOwnerPID as String] as? Int) == %d }.map { ["id": $0[kCGWindowNumber as String]!, "layer": $0[kCGWindowLayer as String]!, "bounds": $0[kCGWindowBounds as String]!] }
let screens = NSScreen.screens.map { ["w": $0.frame.width, "h": $0.frame.height, "scale": $0.backingScaleFactor, "fps": $0.maximumFramesPerSecond] }
print(String(data: try! JSONSerialization.data(withJSONObject: ["windows": windows, "screens": screens]), encoding: .utf8)!)
"""
PROCESS = "tell application \"System Events\" to tell (first process whose unix id is %d)"
KEY = "tell application \"System Events\"\nset frontmost of (first process whose unix id is %d) to true\ndelay 0.5\nkey code %d%s\nend tell"


def settled(rows, sizes, min_fps):
    tail = rows[-2:]
    return len(tail) == 2 and all((row[0], row[1]) in sizes for row in tail) and tail[-1][2] >= min_fps


def check_open(rows, screens):
    sizes = [(round(SIZE[0] * scale), round(SIZE[1] * scale)) for scale in sorted({screen["scale"] for screen in screens})]
    return settled(rows, sizes, MIN_FPS), "rows=%s expected=%s min_fps=%d" % (rows[-4:], sizes, MIN_FPS)


def check_resize(rows, screens):
    ok = False
    scales = sorted({screen["scale"] for screen in screens})
    for scale in scales:
        tail = rows[-2:]
        ok = ok or (len(tail) == 2 and all(row[0] == round(RESIZED[0] * scale) and round((RESIZED[1] - TITLE_BAR) * scale) <= row[1] <= round(RESIZED[1] * scale) for row in tail) and tail[-1][2] > 0)
    return ok, "rows=%s expected=%dx%d logical scales=%s" % (rows[-4:], RESIZED[0], RESIZED[1], scales)


def check_fullscreen(rows, screens):
    sizes = [(round(screen["w"] * screen["scale"]), round(screen["h"] * screen["scale"])) for screen in screens]
    return settled(rows, sizes, 1), "rows=%s screens=%s" % (rows[-4:], sizes)


def check_fps(rows, fps):
    low, high = FPS_BANDS[fps]
    tail = rows[-2:]
    return len(tail) == 2 and all(low <= row[2] <= high for row in tail), "rows=%s fps=%d band=%d-%d" % (rows[-4:], fps, low, high)


def check_act(text, fps):
    reacts = [match.groups() for match in REACT_LINE.finditer(text)]
    mouths = [(int(frames), float(value)) for frames, value in MOUTH_LINE.findall(text)]
    played = any(kind == REACT_KIND and motion != "undefined" for kind, motion in reacts)
    seconds = sum(frames for frames, _ in mouths) / fps
    open_max = max([value for _, value in mouths], default=0.0)
    slept = bool(SLEEP_LINE.search(text))
    ok = played and slept and open_max >= MOUTH_OPEN and abs(seconds - vts_e2e.PCM_SECONDS) <= SPEECH_SLACK
    return ok, "react=%s mouth_rows=%s mouth_seconds=%.2f audio_seconds=%d open_max=%.3f min=%.2f slept=%s" % (reacts, mouths, seconds, vts_e2e.PCM_SECONDS, open_max, MOUTH_OPEN, slept)


def bmp_pixels(data):
    offset, width, height, bits = struct.unpack_from("<I", data, 10)[0], struct.unpack_from("<i", data, 18)[0], struct.unpack_from("<i", data, 22)[0], struct.unpack_from("<H", data, 28)[0]
    if bits != 32:
        raise ValueError("bmp bits=%d, want 32" % bits)
    return offset, width, abs(height), height < 0


def pixel(data, layout, x, y):
    offset, width, height, top_down = layout
    row = y if top_down else height - 1 - y
    start = offset + (row * width + x) * 4
    return data[start:start + 3]


def check_lit(data):
    layout = bmp_pixels(data)
    width, height = layout[1], layout[2]
    if width < 16 or height < 16:
        return False, "image %dx%d" % (width, height)
    background = pixel(data, layout, width // 20, height - 1 - height // 20)
    total = 0
    lit = 0
    for y in range(height // 3, height * 2 // 3, 4):
        for x in range(width // 3, width * 2 // 3, 4):
            total += 1
            lit += sum(abs(a - b) for a, b in zip(pixel(data, layout, x, y), background)) > LIT_DELTA
    fraction = lit / total
    return fraction >= LIT_FRACTION, "image=%dx%d background=%s lit=%.3f min=%.2f" % (width, height, list(background), fraction, LIT_FRACTION)


def make_bmp(width, height, colour):
    body = b"".join(bytes(colour(x, y)) + b"\xff" for y in range(height) for x in range(width))
    return b"BM" + struct.pack("<IHHI", 54 + len(body), 0, 0, 54) + struct.pack("<IiiHHIIiiII", 40, width, -height, 1, 32, 0, len(body), 0, 0, 0, 0) + body


def model_square(x, y):
    if 30 <= x < 70 and 20 <= y < 80:
        return (200, 120, 160)
    return (30, 30, 40)


def selftest(result):
    log = "\n".join([
        "hikari: listening 127.0.0.1:8870 models=12",
        "hikari: window size=1920x1440 fps=41",
        "hikari: window size=1920x1440 fps=60",
        "hikari: window size=1920x1440 fps=60",
    ])
    retina = [{"w": 1920, "h": 1080, "scale": 2, "fps": 60}]
    blank = make_bmp(100, 100, lambda x, y: (30, 30, 40))
    cases = [
        ("parse_rows", ([tuple(int(v) for v in match.groups()) for match in WINDOW_LINE.finditer("noise\n" + log + "\nhikari: window size=x fps=1")] == [(1920, 1440, 41), (1920, 1440, 60), (1920, 1440, 60)], "three rows, junk ignored")),
        ("open_good", check_open([tuple(int(v) for v in match.groups()) for match in WINDOW_LINE.finditer(log)], retina)),
        ("open_wrong_size_rejected", (not check_open([tuple(int(v) for v in match.groups()) for match in WINDOW_LINE.finditer(log.replace("1920x1440", "960x720"))], retina)[0], "points not pixels must fail")),
        ("open_fps0_rejected", (not check_open([tuple(int(v) for v in match.groups()) for match in WINDOW_LINE.finditer(log.replace("fps=60", "fps=0"))], retina)[0], "fps 0 must fail")),
        ("open_one_row_rejected", (not check_open([(1920, 1440, 60)], retina)[0], "one second must fail")),
        ("lit_good", check_lit(make_bmp(100, 100, model_square))),
        ("lit_blank_rejected", (not check_lit(blank)[0], "background only must fail")),
        ("lit_bottom_up_good", check_lit(blank[:22] + struct.pack("<i", 100) + make_bmp(100, 100, model_square)[26:])),
        ("resize_good", check_resize([(1920, 1440, 60), (3200, 1744, 58), (3200, 1744, 60)], retina)),
        ("resize_unchanged_rejected", (not check_resize([(1920, 1440, 60), (1920, 1440, 60)], retina)[0], "old size must fail")),
        ("resize_stalled_rejected", (not check_resize([(3200, 1744, 60), (3200, 1744, 0)], retina)[0], "fps 0 after resize must fail")),
        ("fullscreen_good", check_fullscreen([(3200, 1744, 60), (3840, 2160, 55), (3840, 2160, 60)], retina)),
        ("fullscreen_windowed_rejected", (not check_fullscreen([(3200, 1744, 60), (3200, 1744, 60)], retina)[0], "window size must fail")),
        ("restore_good", (settled([(3840, 2160, 60), (3200, 1744, 60), (3200, 1744, 60)], [(3200, 1744)], 1), "back to windowed size")),
        ("restore_stuck_rejected", (not settled([(3840, 2160, 60), (3840, 2160, 60)], [(3200, 1744)], 1), "still fullscreen must fail")),
        ("fps60_good", check_fps([(1920, 1440, 41), (1920, 1440, 59), (1920, 1440, 60)], 60)),
        ("fps30_good", check_fps([(1920, 1440, 20), (1920, 1440, 30), (1920, 1440, 29)], 30)),
        ("fps60_at_90_rejected", (not check_fps([(1920, 1440, 90), (1920, 1440, 90)], 60)[0], "fps 90 at --fps 60 must fail")),
        ("fps0_rejected", (not check_fps([(1920, 1440, 60), (1920, 1440, 0)], 60)[0], "fps 0 must fail")),
        ("act_good", check_act("hikari: action pet motion=none mid_reply=false\nhikari: window mouth frames=60 open_max=0.900\nhikari: window mouth frames=60 open_max=1.000\nhikari: window mouth frames=60 open_max=0.800\nhikari: asleep\nhikari: window size=1920x1440 fps=60\nhikari: wake", 60)),
        ("act_no_wake_rejected", (not check_act("hikari: action pet motion=none mid_reply=false\nhikari: window mouth frames=60 open_max=0.900\nhikari: window mouth frames=60 open_max=1.000\nhikari: window mouth frames=60 open_max=0.800\nhikari: window sleep seconds=1", 60)[0], "sleep that never wakes must fail")),
        ("act_still_mouth_rejected", (not check_act("hikari: action pet motion=none mid_reply=false\nhikari: window mouth frames=60 open_max=0.000\nhikari: window mouth frames=60 open_max=0.000\nhikari: window mouth frames=60 open_max=0.000", 60)[0], "mouth never opening must fail")),
        ("act_no_react_rejected", (not check_act("hikari: other line\nhikari: window mouth frames=60 open_max=0.900\nhikari: window mouth frames=60 open_max=0.900\nhikari: window mouth frames=60 open_max=0.900", 60)[0], "unplayed react must fail")),
    ]
    for name, outcome in cases:
        result.add(name, outcome)


def free_port():
    for port in PORTS:
        with socket.socket() as probe:
            try:
                probe.bind(("127.0.0.1", port))
            except OSError:
                continue
        return port
    raise RuntimeError("no free port in %d-%d" % (PORTS[0], PORTS[-1]))


def start(binary, extra, log_path):
    port = free_port()
    handle = vts_e2e.start_server(binary, port, log_path, ["--window"] + extra, MODEL, {"size": list(SIZE)})
    handle.update(path=log_path, port=port)
    return handle


def log_size(handle):
    handle["log"].flush()
    return os.path.getsize(handle["path"])


def wait_check(handle, since, check, seconds):
    deadline = time.time() + seconds
    while True:
        with open(handle["path"]) as reader:
            reader.seek(since)
            outcome = check([tuple(int(v) for v in match.groups()) for match in WINDOW_LINE.finditer(reader.read())])
        if outcome[0] or time.time() >= deadline:
            return outcome
        if handle["proc"].poll() is not None:
            return False, "process exited %s; %s" % (handle["proc"].returncode, outcome[1])
        time.sleep(0.5)


def tool(command):
    done = subprocess.run(command, capture_output=True, text=True, timeout=TOOL_SECONDS)
    if done.returncode != 0:
        raise RuntimeError("%s failed %d: %s" % (command[0], done.returncode, done.stderr.strip()))
    return done.stdout


def wait_window(pid):
    deadline = time.time() + START_SECONDS
    while time.time() < deadline:
        info = json.loads(tool(["swift", "-e", WINDOW_INFO % pid]))
        normal = [window for window in info["windows"] if window["layer"] == 0]
        if normal:
            return normal[0]["id"], info["screens"]
        time.sleep(1)
    raise RuntimeError("no window for pid %d within %ds" % (pid, START_SECONDS))


def capture(window_id, path):
    subprocess.run(["caffeinate", "-u", "-t", "2"], timeout=TOOL_SECONDS)
    tool(["screencapture", "-x", "-o", "-t", "bmp", "-l", str(window_id), path])
    with open(path, "rb") as reader:
        return reader.read()


def wait_exit(proc):
    try:
        return proc.wait(timeout=EXIT_SECONDS)
    except subprocess.TimeoutExpired:
        return None


def windowed(args, result):
    handle = start(args.binary, [], args.out + ".log")
    try:
        pid = handle["proc"].pid
        window_id, screens = wait_window(pid)
        result.add("1_open", wait_check(handle, 0, lambda rows: check_open(rows, screens), START_SECONDS))
        result.add("2_lit", check_lit(capture(window_id, args.out + ".bmp")))
        since = log_size(handle)
        tool(["osascript", "-e", PROCESS % pid + " to set position of window 1 to {0, 30}"])
        tool(["osascript", "-e", PROCESS % pid + " to set size of window 1 to {%d, %d}" % RESIZED])
        result.add("3_resize", wait_check(handle, since, lambda rows: check_resize(rows, screens), WAIT_SECONDS))
        with open(handle["path"]) as reader:
            rows = [tuple(int(v) for v in match.groups()) for match in WINDOW_LINE.finditer(reader.read())]
        windowed_size = (rows[-1][0], rows[-1][1]) if rows else (0, 0)
        since = log_size(handle)
        tool(["osascript", "-e", KEY % (pid, 36, " using {option down}")])
        result.add("4_fullscreen", wait_check(handle, since, lambda rows: check_fullscreen(rows, screens), WAIT_SECONDS))
        since = log_size(handle)
        tool(["osascript", "-e", KEY % (pid, 53, "")])
        result.add("4_restore", wait_check(handle, since, lambda rows: (settled(rows, [windowed_size], 1), "rows=%s expected=%s" % (rows[-4:], windowed_size)), WAIT_SECONDS))
        since = log_size(handle)
        port = handle["port"]
        vts_e2e.http_call(port, "POST", "/action?action=%s" % REACT_KIND)
        vts_e2e.read_frames(port, "POST", "/talk?fps=%d&codec=rgb565&size=160x120" % vts_e2e.FPS, vts_e2e.FPS * vts_e2e.PCM_SECONDS * 2, vts_e2e.wav(vts_e2e.synth_pcm()))
        time.sleep(vts_e2e.PCM_SECONDS + 2)
        vts_e2e.http_call(port, "POST", "/action?action=sleep")
        time.sleep(SLEEP_SECONDS + 1)
        vts_e2e.http_call(port, "POST", "/action?action=look_left")
        time.sleep(2)
        with open(handle["path"]) as reader:
            reader.seek(since)
            text = reader.read()
        result.add("7_act", check_act(text, ([tuple(int(v) for v in match.groups()) for match in WINDOW_LINE.finditer(text)] or [(0, 0, MIN_FPS)])[-1][2]))
        for label, key, extra, line in KEY_CHECKS:
            since = log_size(handle)
            tool(["osascript", "-e", KEY % (pid, key, extra)])
            time.sleep(1)
            with open(handle["path"]) as reader:
                reader.seek(since)
                found = line % MODEL in reader.read()
            result.add(label, (found, "key=%d%s wanted %r" % (key, extra, line % MODEL)))
        since = log_size(handle)
        vts_e2e.switch_model(port, OTHER_MODEL)
        time.sleep(3)
        with open(handle["path"]) as reader:
            reader.seek(since)
            text = reader.read()
        rows = [tuple(int(v) for v in match.groups()) for match in WINDOW_LINE.finditer(text)]
        follows = "hikari: window model=%s" % OTHER_MODEL in text
        result.add("8_follows_switch", (follows and bool(rows) and rows[-1][2] >= MIN_FPS, "window_model_line=%s rows=%s" % (follows, rows[-2:])))
        tool(["osascript", "-e", PROCESS % pid + " to click (first button of window 1 whose subrole is \"AXCloseButton\")"])
        code = wait_exit(handle["proc"])
        result.add("6_close_exit", (code == 0, "returncode=%s within %ds" % (code, EXIT_SECONDS)))
    finally:
        vts_e2e.stop_server(handle)


def paced(args, result, fps):
    handle = start(args.binary, ["--fps", str(fps)], args.out + ".fps%d.log" % fps)
    try:
        wait_window(handle["proc"].pid)
        result.add("5_fps%d" % fps, wait_check(handle, 0, lambda rows: check_fps(rows, fps), START_SECONDS))
    finally:
        vts_e2e.stop_server(handle)


def run(args, result):
    made = []
    try:
        for name, source in ((MODEL, SOURCE_MODEL), (OTHER_MODEL, vts_e2e.SOURCE_MODEL)):
            target = os.path.join(REPO, "models", name)
            if not os.path.exists(target):
                made.append(target)
                shutil.copytree(os.path.join(SAMPLES, source), target)
                os.rename(os.path.join(target, source + ".model3.json"), os.path.join(target, name + ".model3.json"))
        windowed(args, result)
        for fps in FPS_BANDS:
            paced(args, result, fps)
    finally:
        for target in made:
            shutil.rmtree(target, ignore_errors=True)


def main():
    global PORTS
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary")
    parser.add_argument("--out")
    parser.add_argument("--selftest", action="store_true")
    parser.add_argument("--ports", help="first-last port range, default %d-%d" % (PORTS[0], PORTS[-1]))
    args = parser.parse_args()
    if args.ports:
        first, last = (int(port) for port in args.ports.split("-"))
        PORTS = range(first, last + 1)
    if not args.selftest and not (args.binary and args.out):
        parser.error("--binary and --out are required without --selftest")
    if LIVE_PORT in PORTS:
        parser.error("port %d is the live service" % LIVE_PORT)
    result = vts_e2e.Result(args.out)
    try:
        if args.selftest:
            selftest(result)
        else:
            run(args, result)
    except BaseException as error:
        result.payload["error"] = "%s: %s" % (type(error).__name__, error)
        result.flush()
        raise
    return vts_e2e.finish(result)


if __name__ == "__main__":
    sys.exit(main())
