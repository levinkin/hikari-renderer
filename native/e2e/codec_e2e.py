#!/usr/bin/env python3
import argparse
import http.client
import io
import os
import shutil
import subprocess
import sys
import tempfile
import threading
import time

from PIL import Image

import vts_e2e

PORT = 9325
MODEL = "codechiyori%d" % os.getpid()
SLEEP_SECONDS = 6.0
QOI_MAE = 6.0
MJPEG_FPS = 12
MJPEG_PARTS = 24
SIZES = [(960, 720), (160, 120)]
PROBE = """#[path = "CODEC_PATH"]
mod codec;

fn main() {
    for (width, height) in [(160usize, 120usize), (960, 720)] {
        let rgba: Vec<u8> = (0..width * height * 4).map(|i| ((i * 7 + i / 977 * 13) % 251) as u8).collect();
        for name in ["pal256", "jpeg", "rgb565", "split"] {
            let data = codec::encode(&rgba, width, height, name);
            let hash = data.iter().fold(1469598103934665603u64, |hash, byte| (hash ^ *byte as u64).wrapping_mul(1099511628211));
            println!("{}x{}:{}:{}:{:016x}", width, height, name, data.len(), hash);
        }
    }
}
"""
QOI_PROBE = """#[path = "CODEC_PATH"]
mod codec;

fn main() {
    for (width, height) in [(960usize, 720usize), (160, 120)] {
        let rgba: Vec<u8> = (0..width * height * 4).map(|i| ((i * 7 + i / 977 * 13) % 251) as u8).collect();
        let started = std::time::Instant::now();
        for _ in 0..20 {
            codec::encode(&rgba, width, height, "qoi");
        }
        println!("{}x{}_qoi_encode_ms={:.2}", width, height, started.elapsed().as_secs_f64() * 1000.0 / 20.0);
    }
}
"""


def stream(port, codec, size, count):
    return vts_e2e.read_frames(port, "GET", "/listen?fps=24&codec=%s&size=%dx%d" % (codec, size[0], size[1]), count)[1]


def asleep_frames(port, codecs, size):
    vts_e2e.http_call(port, "POST", "/action?action=sleep")
    time.sleep(SLEEP_SECONDS)
    found = {}
    threads = [threading.Thread(target=grab, args=(port, codec, size, found)) for codec in codecs]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
    return found


def grab(port, codec, size, found):
    found[codec] = stream(port, codec, size, 6)[-1]


def mae(first, second):
    return sum(abs(a - b) for a, b in zip(first, second)) / len(first)


def check_qoi(port):
    details = []
    ok = True
    for size in SIZES:
        frames = asleep_frames(port, ["qoi", "jpeg"], size)
        qoi = Image.open(io.BytesIO(frames["qoi"]))
        jpeg = Image.open(io.BytesIO(frames["jpeg"]))
        error = mae(qoi.convert("RGB").tobytes(), jpeg.convert("RGB").tobytes())
        ok = ok and qoi.format == "QOI" and qoi.size == size and error < QOI_MAE
        details.append("%dx%d format=%s mae_vs_jpeg=%.2f" % (size[0], size[1], qoi.format, error))
    return ok, "; ".join(details) + " need mae<%.1f" % QOI_MAE


def check_mjpeg(port, size):
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
    started = time.monotonic()
    conn.request("GET", "/listen?fps=%d&codec=mjpeg&size=%dx%d" % (MJPEG_FPS, size[0], size[1]))
    resp = conn.getresponse()
    kind = resp.getheader("content-type")
    parts = []
    while resp.status == 200 and len(parts) < MJPEG_PARTS:
        boundary = resp.fp.readline()
        headers = {}
        line = resp.fp.readline()
        while line not in (b"\r\n", b""):
            key, value = line.decode().split(":", 1)
            headers[key.strip().lower()] = value.strip()
            line = resp.fp.readline()
        data = resp.fp.read(int(headers["content-length"]))
        resp.fp.readline()
        image = Image.open(io.BytesIO(data))
        image.load()
        parts.append((boundary.strip(), headers.get("content-type"), image.format, image.size))
    seconds = time.monotonic() - started
    conn.close()
    good = all(part == (b"--frame", "image/jpeg", "JPEG", size) for part in parts)
    ok = kind == "multipart/x-mixed-replace; boundary=frame" and len(parts) == MJPEG_PARTS and good and 1.5 <= seconds <= 2.6
    return ok, "content-type=%s parts=%d all_jpeg_%dx%d=%s seconds=%.2f" % (kind, len(parts), size[0], size[1], good, seconds)


def check_unknown(port):
    status, body = vts_e2e.http_call(port, "GET", "/listen?fps=24&codec=webp")
    talk, _ = vts_e2e.http_call(port, "POST", "/talk?fps=24&codec=webp", vts_e2e.wav(vts_e2e.synth_pcm()))
    return status == 400 and talk == 400, "listen=%d talk=%d" % (status, talk)


def check_fps(port):
    bad = []
    for value in ["24.5", "0", "241", "abc", "-1"]:
        status, body = vts_e2e.http_call(port, "GET", "/listen?codec=jpeg&size=160x120&fps=%s" % value)
        if (status, body.decode().strip()) != (400, "hikari: fps must be a whole number from 1 to 240, got %s" % value):
            bad.append((value, status, body))
    status, body = vts_e2e.http_call(port, "GET", "/listen?codec=jpeg&size=160x120&fps=24&fps=24")
    if (status, body.decode().strip()) != (400, "hikari: fps given more than once"):
        bad.append(("repeated", status, body))
    talk, _ = vts_e2e.http_call(port, "POST", "/talk?codec=jpeg&size=160x120&fps=241", vts_e2e.wav(vts_e2e.synth_pcm()))
    if talk != 400:
        bad.append(("talk241", talk))
    rates = {}
    for path, want in (("/listen?codec=rgb565&size=160x120&fps=240", 240), ("/listen?codec=rgb565&size=160x120", 60)):
        status, frames, _, times = vts_e2e.read_frames(port, "GET", path, want * 3)
        times = times[want:]
        rates[want] = (len(times) - 1) / (times[-1] - times[0]) if len(times) > 1 else 0.0
    ok = not bad and abs(rates[240] - 240) <= 4.8 and abs(rates[60] - 60) <= 1.2
    return ok, "bad=%s fps240_measured=%.1f default_measured=%.1f (2 s after a 1 s warmup, want 240 and 60 within 2%%)" % (bad, rates[240], rates[60])


def check_port_arg(binary, port, log_path):
    handle = vts_e2e.start_server(binary, port + 2, log_path, ["--port", str(port + 2)], MODEL, {"port": port + 1})
    try:
        status, _ = vts_e2e.http_call(port + 2, "GET", "/models")
    finally:
        vts_e2e.stop_server(handle)
    bad = {}
    for value in ["0", "65536", "abc", "80.5"]:
        run = subprocess.run([binary, "--port", value], capture_output=True, text=True, timeout=20)
        if run.returncode != 2 or run.stderr.strip() != "hikari: --port must be 1 to 65535, got %s" % value:
            bad[value] = (run.returncode, run.stderr.strip())
    return status == 200 and not bad, "settings_port=%d --port=%d status=%d bad=%s" % (port + 1, port + 2, status, bad)


def qoi_numbers(port):
    rows = qoi_encode_ms()
    for size in SIZES:
        frames = stream(port, "qoi", size, 24)
        rows.append("%dx%d bytes/frame=%d" % (size[0], size[1], sum(map(len, frames)) // len(frames)))
    return True, "; ".join(rows)


def encoder_hashes(codec_path, scratch, label, source=None):
    probe = os.path.join(scratch, label + ".rs")
    with open(probe, "w") as writer:
        writer.write((source or PROBE).replace("CODEC_PATH", codec_path))
    subprocess.run(["rustc", "-O", "--edition", "2021", "-A", "warnings", probe, "-o", probe[:-3]], check=True)
    return subprocess.run([probe[:-3]], check=True, capture_output=True, text=True).stdout.split()


def qoi_encode_ms():
    scratch = tempfile.mkdtemp(prefix="hikari-qoi-")
    try:
        return encoder_hashes(os.path.join(vts_e2e.REPO, "native", "server", "src", "codec.rs"), scratch, "qoi", QOI_PROBE)
    finally:
        shutil.rmtree(scratch, ignore_errors=True)


def check_encoders_unchanged(rev):
    scratch = tempfile.mkdtemp(prefix="hikari-codec-")
    try:
        before = os.path.join(scratch, "codec_before.rs")
        with open(before, "w") as writer:
            writer.write(subprocess.run(["git", "-C", vts_e2e.REPO, "show", "%s:native/server/src/codec.rs" % rev], check=True, capture_output=True, text=True).stdout)
        old = encoder_hashes(before, scratch, "before")
        new = encoder_hashes(os.path.join(vts_e2e.REPO, "native", "server", "src", "codec.rs"), scratch, "after")
    finally:
        shutil.rmtree(scratch, ignore_errors=True)
    return old == new and len(old) == 8, "before=%s after=%s" % (old, new)


def run(args, result):
    target = os.path.join(vts_e2e.REPO, "models", MODEL)
    os.mkdir(target)
    handle = None
    try:
        vts_e2e.make_copy(target, MODEL, "Hiyori", {}, still=True)
        result.add("1_encoders_unchanged", check_encoders_unchanged(args.before_rev))
        result.add("7_port_arg", check_port_arg(args.binary, args.port, args.out + ".port.log"))
        handle = vts_e2e.start_server(args.binary, args.port, args.out + ".log", model=MODEL, settings={"idle_actions": [], "action_every": 1000})
        result.add("2_qoi_decodes", check_qoi(args.port))
        result.add("3_mjpeg_multipart", check_mjpeg(args.port, (320, 240)))
        result.add("4_unknown_codec", check_unknown(args.port))
        result.add("5_qoi_bytes", qoi_numbers(args.port))
        result.add("6_fps", check_fps(args.port))
    finally:
        if handle:
            vts_e2e.stop_server(handle)
        shutil.rmtree(target, ignore_errors=True)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True)
    parser.add_argument("--before-rev", default="HEAD~1")
    parser.add_argument("--out", required=True)
    parser.add_argument("--port", type=int, default=PORT)
    args = parser.parse_args()
    result = vts_e2e.Result(args.out)
    run(args, result)
    result.flush()
    return vts_e2e.finish(result)


if __name__ == "__main__":
    sys.exit(main())
