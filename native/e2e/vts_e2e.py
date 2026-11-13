#!/usr/bin/env python3
import argparse
import bisect
import concurrent.futures
import hashlib
import http.client
import json
import math
import os
import re
import shutil
import signal
import socket
import statistics
import struct
import subprocess
import sys
import tempfile
import threading
import time
import urllib.parse

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
SAMPLES = os.path.join(REPO, "sdk", "Samples", "Resources")
if not os.path.isdir(SAMPLES):
    SAMPLES = os.path.join(REPO, "native", "server", "target", "samples")
LIVE_PORT = 8793
PORT = 9320
FPS = 24
RATE = 24000
PCM_SECONDS = 3
WIDTH = 160
HEIGHT = 120
TIMEOUT = 180
PLAIN_MODEL = "Haru"
SOURCE_MODEL = "Mao"
WIND_MODEL = "Hiyori"
WIND_MARGIN = 1.0
GUST_SECONDS = 8
GUST_MARGIN = 0.3
ROUTES = [("MouthOpen", "ParamA")]
REQUIRED_REMAPS = [("ParamMouthOpenY", "ParamA")]
MOUTH_ROWS = (98, 110)
MOUTH_COLS = (70, 92)
MOTION_MARGIN = 1.0
VTS_SOURCE = os.path.join(REPO, "models", "hiyori")
VTS_COPY = "hiyori_vts"
VTS_IDLE = "hiyori_m02"
VTS_HOTKEY = ("My Animation 1", "hiyori_m03")
VTS_SETUP = ("21", "0", "50,0")
FOLLOW_ID = "ParamAngleX"
FOLLOW_R = 0.9
FOLLOW_RANGE = 2.0
FOLLOW_WINDOW = 2.5
FOLLOW_SHARE = 0.6
REACT_SKIP = 1.0
REACT_SECONDS = 3.0
VTUBE_LINE = re.compile(r"hikari: vtube model=(\S+) params=(\d+) expressions=(\d+) physics=(\S+)")
REMAP_LINE = re.compile(r"hikari: vtube remap model=(\S+) (\S+)->(\S+)")
VIEW_LINE = re.compile(r"hikari: view model=(\S+) zoom=(\S+) x=(\S+) y=(\S+) rot=(\S+)")
FRAMED_POSITION = {"Position": {"x": 16, "y": -12, "z": 0}, "Rotation": {"x": 0, "y": 0, "z": -0.10452846326765347, "w": 0.9945218953682733}, "Scale": {"x": 1.2, "y": 1.2, "z": 1}}
FRAMED_VIEW = (0.549, 0.1928, 0.4819, -12.0)
VTS_VIEW = (0.3637, 0.0, -0.0559, 0.0)
SAVED_VIEW = (2.6, 0.1, -0.2, 0.0)
IDENTITY_VIEW = (1.0, 0.0, 0.0, 0.0)
VIEW_TOLERANCE = 0.001
SWITCH_SECONDS = 3
SWITCH_SETTLE = 0.5
SWITCH_FLOOR = 8.0
SWITCH_RATIO = 3.0
SHARED_SECONDS = 5
SHARED_RANGE = 20
HEAD_TOLERANCE = 10
HEAD_SHARE = 0.9
CPU_SECONDS = 5
CPU_RATIO = 1.5
DEFAULT_SIZE = (960, 720)
SIZE_ERROR = "hikari: size must be WxH up to 3840x2160, got %s"
RGB565 = [bytes((((key >> 11) << 3) | (key >> 13), (((key >> 5) & 63) << 2) | ((key >> 9) & 3), ((key & 31) << 3) | ((key >> 2) & 7))) for key in range(65536)]


def http_call(port, method, path, body=None):
    deadline = time.monotonic() + TIMEOUT
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=TIMEOUT)
    conn.connect()
    watchdog = threading.Timer(max(0.0, deadline - time.monotonic()), conn.sock.shutdown, (socket.SHUT_RDWR,))
    watchdog.start()
    try:
        conn.request(method, path, body=body)
        resp = conn.getresponse()
        payload = resp.read()
    finally:
        watchdog.cancel()
        conn.close()
    if time.monotonic() >= deadline:
        raise TimeoutError("%s %s exceeded %ds" % (method, path, TIMEOUT))
    return resp.status, payload


def read_exact(resp, count):
    buf = bytearray()
    while len(buf) < count:
        chunk = resp.read(count - len(buf))
        if not chunk:
            break
        buf += chunk
    return bytes(buf)


def read_frames(port, method, path, count, body=None):
    deadline = time.monotonic() + TIMEOUT
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=TIMEOUT)
    conn.connect()
    watchdog = threading.Timer(max(0.0, deadline - time.monotonic()), conn.sock.shutdown, (socket.SHUT_RDWR,))
    watchdog.start()
    try:
        conn.request(method, path, body=body)
        resp = conn.getresponse()
        frames = []
        heads = []
        times = []
        while resp.status == 200 and len(frames) < count:
            head = read_exact(resp, 13)
            if len(head) < 13:
                break
            length = struct.unpack(">II", head[:8])[1]
            data = read_exact(resp, length - 1)
            if len(data) < length - 1:
                break
            frames.append(data)
            heads.append(struct.unpack(">hh", head[8:12]))
            times.append(time.monotonic())
    finally:
        watchdog.cancel()
        conn.close()
    if time.monotonic() >= deadline:
        raise TimeoutError("%s %s exceeded %ds" % (method, path, TIMEOUT))
    return resp.status, frames, heads, times


def lz4_decode(src, limit):
    out = bytearray()
    pos = 0
    size = len(src)
    while pos < size and len(out) < limit:
        token = src[pos]
        pos += 1
        literal = token >> 4
        if literal == 15:
            while True:
                extra = src[pos]
                pos += 1
                literal += extra
                if extra != 255:
                    break
        out += src[pos:pos + literal]
        pos += literal
        if pos >= size:
            break
        offset = src[pos] | (src[pos + 1] << 8)
        pos += 2
        match = token & 15
        if match == 15:
            while True:
                extra = src[pos]
                pos += 1
                match += extra
                if extra != 255:
                    break
        match += 4
        start = len(out) - offset
        period = out[start:start + match]
        out += (period * (match // len(period) + 1))[:match]
    return bytes(out)


def decode_rgb565(data):
    raw = lz4_decode(data, WIDTH * HEIGHT * 2)
    if len(raw) < WIDTH * HEIGHT * 2:
        raise ValueError("rgb565 payload short: %d of %d" % (len(raw), WIDTH * HEIGHT * 2))
    return b"".join([RGB565[key] for key in struct.unpack("<%dH" % (WIDTH * HEIGHT), raw[:WIDTH * HEIGHT * 2])])


def speech_motion(images):
    sums = [[0] * (WIDTH * HEIGHT * 3), [0] * (WIDTH * HEIGHT * 3)]
    counts = [0, 0]
    for index, image in enumerate(images):
        phase = (index * 2 // FPS) % 2
        counts[phase] += 1
        for y in range(*MOUTH_ROWS):
            for x in range((y * WIDTH + MOUTH_COLS[0]) * 3, (y * WIDTH + MOUTH_COLS[1]) * 3):
                sums[phase][x] += image[x]
    if not all(counts):
        return 0.0
    diffs = [abs(a / counts[1] - b / counts[0]) for a, b in zip(sums[1], sums[0])]
    return sum(diffs) / ((MOUTH_ROWS[1] - MOUTH_ROWS[0]) * (MOUTH_COLS[1] - MOUTH_COLS[0]) * 3)


def mean_image(images):
    count = len(images)
    if not count:
        return []
    return [sum(column) / count for column in zip(*images)]


def image_diff(first, second):
    if not first or len(first) != len(second):
        return 0.0
    return sum(abs(a - b) for a, b in zip(first, second)) / len(first)


def mouth_spread(images):
    crops = [[image[x] for y in range(*MOUTH_ROWS) for x in range((y * WIDTH + MOUTH_COLS[0]) * 3, (y * WIDTH + MOUTH_COLS[1]) * 3)] for image in images]
    if len(crops) < 2:
        return 0.0
    return statistics.mean(statistics.pstdev(column) for column in zip(*crops))


def jpeg_size(data):
    pos = 2
    while pos + 9 <= len(data) and data[pos] == 0xFF:
        marker = data[pos + 1]
        if 0xC0 <= marker <= 0xCF and marker not in (0xC4, 0xC8, 0xCC):
            height, width = struct.unpack(">HH", data[pos + 5:pos + 9])
            return width, height
        pos += 2 + struct.unpack(">H", data[pos + 2:pos + 4])[0]
    return 0, 0


def cputime_seconds(text):
    days, _, clock = text.rpartition("-")
    total = 0.0
    for part in clock.split(":"):
        total = total * 60 + float(part)
    return total + float(days or 0) * 86400


def process_usage(pid):
    rss, cputime = subprocess.run(["ps", "-o", "rss=,cputime=", "-p", str(pid)], capture_output=True, text=True, check=True).stdout.split()
    return int(rss), cputime_seconds(cputime)


def vtube_rows(log, model):
    rows = []
    for match in VTUBE_LINE.finditer(log):
        if match.group(1) == model:
            rows.append(match.groups()[1:])
    return rows


def remaps(log, model):
    found = set()
    for match in REMAP_LINE.finditer(log):
        if match.group(1) == model:
            found.add((match.group(2), match.group(3)))
    return sorted(found)


def check_framing(log, model, start, saved):
    rows = [tuple(float(v) for v in match.groups()[1:]) for match in VIEW_LINE.finditer(log) if match.group(1) == model]
    near = [len(rows) > i and all(abs(a - b) <= VIEW_TOLERANCE for a, b in zip(rows[i], view)) for i, view in ((0, start), (1, saved), (-1, IDENTITY_VIEW))]
    return len(rows) >= 3 and all(near), "model=%s views=%s" % (model, rows)


def check_vts_copy(log):
    rows = vtube_rows(log, VTS_COPY)
    found = remaps(log, VTS_COPY)
    ok = bool(rows) and all(row == VTS_SETUP for row in rows) and not found
    return ok, "vtube_lines=%s remaps=%s" % (rows, found)


def check_plain(log, model, images):
    rows = vtube_rows(log, model)
    found = remaps(log, model)
    lit = any(len(set(image)) > 1 for image in images)
    ok = len(images) >= FPS and lit and all(row[:2] == ("0", "0") for row in rows) and not found
    return ok, "frames=%d lit=%s vtube_lines=%s remaps=%s" % (len(images), lit, rows, found)


def check_remap(log, model, copy_images, source_images):
    rows = vtube_rows(log, model)
    found = remaps(log, model)
    missing = [pair for pair in REQUIRED_REMAPS if pair not in found]
    moved = speech_motion(copy_images)
    still = speech_motion(source_images)
    ok = bool(rows) and all(row[:2] == (str(len(ROUTES)), "0") for row in rows) and not missing and len(copy_images) >= FPS and moved > still + MOTION_MARGIN
    return ok, "vtube_lines=%s remaps=%s missing=%s motion_copy=%.3f motion_source=%.3f margin=%.2f" % (rows, found, missing, moved, still, MOTION_MARGIN)


def drift(images):
    pairs = list(zip(images, images[FPS // 2:]))
    if not pairs:
        return 0.0
    total = 0
    for first, second in pairs:
        total += sum(abs(a - b) for a, b in zip(first, second))
    return total / (len(pairs) * WIDTH * HEIGHT * 3)


def check_wind(log, windy_model, calm_model, windy_images, calm_images):
    windy_rows = vtube_rows(log, windy_model)
    calm_rows = vtube_rows(log, calm_model)
    moved = drift(windy_images)
    still = drift(calm_images)
    rows_ok = bool(windy_rows) and bool(calm_rows) and all(row[2] == "50,100" for row in windy_rows) and all(row[2] == "50,0" for row in calm_rows)
    ok = rows_ok and len(windy_images) >= FPS * 2 and len(calm_images) >= FPS * 2 and moved > still + WIND_MARGIN
    return ok, "windy_lines=%s calm_lines=%s drift_wind=%.3f drift_calm=%.3f margin=%.2f" % (windy_rows, calm_rows, moved, still, WIND_MARGIN)


def check_muted(log, muted_model, windy_images, muted_images, calm_images):
    rows = vtube_rows(log, muted_model)
    moved = drift(windy_images)
    muted = drift(muted_images)
    still = drift(calm_images)
    ok = bool(rows) and all(row[2] == "50,100" for row in rows) and len(muted_images) >= FPS * 2 and muted < still + WIND_MARGIN and moved > muted + WIND_MARGIN
    return ok, "muted_lines=%s drift_wind=%.3f drift_muted=%.3f drift_calm=%.3f margin=%.2f" % (rows, moved, muted, still, WIND_MARGIN)


def per_second(images):
    return [drift(images[start:start + FPS]) for start in range(0, len(images) - FPS + 1, FPS)]


def check_gusts(windy_images, calm_images):
    windy = per_second(windy_images)
    calm = per_second(calm_images)
    if len(windy) < GUST_SECONDS or len(calm) < GUST_SECONDS:
        return False, "seconds=%d/%d" % (len(windy), len(calm))
    windy_spread = statistics.pstdev(windy)
    calm_spread = statistics.pstdev(calm)
    ok = windy_spread > calm_spread + GUST_MARGIN
    return ok, "spread_wind=%.3f spread_calm=%.3f margin=%.2f per_second_wind=%s per_second_calm=%s" % (windy_spread, calm_spread, GUST_MARGIN, [round(v, 3) for v in windy], [round(v, 3) for v in calm])


def motion_curve(path, param):
    with open(path) as reader:
        motion = json.load(reader)
    for curve in motion["Curves"]:
        if curve["Id"] == param:
            return motion["Meta"]["Duration"], curve["Segments"]
    return motion["Meta"]["Duration"], [0.0, 0.0]


def curve_at(segments, t):
    prev = (segments[0], segments[1])
    pos = 2
    while pos < len(segments):
        kind = segments[pos]
        width = 7 if kind == 1 else 3
        end = (segments[pos + width - 2], segments[pos + width - 1])
        if t <= end[0]:
            u = (t - prev[0]) / (end[0] - prev[0]) if end[0] > prev[0] else 1.0
            if kind == 0:
                return prev[1] + (end[1] - prev[1]) * u
            if kind == 2:
                return prev[1]
            if kind == 3:
                return end[1]
            values = (prev[1], segments[pos + 2], segments[pos + 4], end[1])
            return (1 - u) ** 3 * values[0] + 3 * (1 - u) ** 2 * u * values[1] + 3 * (1 - u) * u ** 2 * values[2] + u ** 3 * values[3]
        prev = end
        pos += width
    return prev[1]


def best_fit(angles, curves):
    best = -1.0
    for duration, segments in curves:
        for step in range(int(duration * FPS)):
            model = [curve_at(segments, (step / FPS + i / FPS) % duration) for i in range(len(angles))]
            try:
                best = max(best, statistics.correlation(angles, model))
            except statistics.StatisticsError:
                best = max(best, 0.0)
    return best


def check_follows(heads, curves, window):
    angles = [-yaw / 10.0 for yaw, _ in heads]
    size = int(window * FPS)
    fits = [round(best_fit(angles[start:start + size], curves), 3) for start in range(0, len(angles) - size + 1, size)]
    spread = max(angles) - min(angles) if angles else 0.0
    share = sum(fit >= FOLLOW_R for fit in fits) / len(fits) if fits else 0.0
    ok = spread >= FOLLOW_RANGE and share >= FOLLOW_SHARE
    return ok, "frames=%d range=%.2f fits=%s share=%.2f need_r=%.2f" % (len(angles), spread, fits, share, FOLLOW_R)


def check_png(branch, main):
    first = hashlib.sha256(branch).hexdigest()
    second = hashlib.sha256(main).hexdigest()
    return bool(branch) and first == second, "branch=%s main=%s bytes=%d/%d" % (first[:16], second[:16], len(branch), len(main))


def wav(pcm, rate=RATE, channels=1, size=None):
    data_size = len(pcm) if size is None else size
    fmt = struct.pack("<HHIIHH", 1, channels, rate, rate * channels * 2, channels * 2, 16)
    return b"RIFF" + struct.pack("<I", (36 + len(pcm)) & 0xFFFFFFFF) + b"WAVE" + b"fmt " + struct.pack("<I", len(fmt)) + fmt + b"data" + struct.pack("<I", data_size) + pcm


def synth_pcm():
    buf = bytearray()
    for i in range(RATE * PCM_SECONDS):
        value = 0
        if int(i // (RATE * 0.5)) % 2 == 0:
            value = int(12000 * math.sin(2 * math.pi * 440 * i / RATE))
        buf += struct.pack("<h", value)
    return bytes(buf)


class Result:
    def __init__(self, path):
        self.path = path
        self.payload = {"checks": [], "error": None}

    def add(self, name, outcome):
        self.payload["checks"].append({"check": name, "pass": bool(outcome[0]), "detail": outcome[1]})
        self.flush()

    def flush(self):
        if not self.path:
            return
        with open(self.path, "w") as handle:
            json.dump(self.payload, handle, indent=1)


def finish(result):
    for check in result.payload["checks"]:
        print("%s %s %s" % ("PASS" if check["pass"] else "FAIL", check["check"], check["detail"]))
    return 0 if all(check["pass"] for check in result.payload["checks"]) else 1


def lz4_literal(data):
    out = bytearray()
    if len(data) < 15:
        out.append(len(data) << 4)
    else:
        out.append(0xF0)
        rest = len(data) - 15
        while rest >= 255:
            out.append(255)
            rest -= 255
        out.append(rest)
    return bytes(out) + data


def canned_frame(shade):
    key = ((shade >> 3) << 11) | ((shade >> 2) << 5) | (shade >> 3)
    return lz4_literal(struct.pack("<H", key) * (WIDTH * HEIGHT - 1) + struct.pack("<H", 0))


def selftest(result):
    log = "\n".join([
        "hikari: page ready gpu=Apple M4",
        "hikari: vtube model=hiyori_vts params=21 expressions=0 physics=50,0",
        "hikari: listening 127.0.0.1:8795 models=12",
        "hikari: vtube model=Haru params=0 expressions=0 physics=none",
        "hikari: vtube model=vtse2e1 params=1 expressions=0 physics=none",
        "hikari: vtube remap model=vtse2e1 ParamMouthOpenY->ParamA",
        "hikari: vtube model=vtse2e1 params=1 expressions=0 physics=none",
        "hikari: vtube model=vtse2e1w100 params=0 expressions=0 physics=50,100",
        "hikari: vtube model=vtse2e1w0 params=0 expressions=0 physics=50,0",
        "hikari: vtube model=vtse2e1m0 params=0 expressions=0 physics=50,100",
        "hikari: view model=hiyori_vts zoom=0.36371255 x=-0.0000000000015118746 y=-0.055881098 rot=-0.0000000000000000000066410922",
        "hikari: view model=vtse2e1f zoom=0.549 x=0.19281 y=0.48188 rot=-12.000001",
        "hikari: view model=vtse2e1f zoom=2.6 x=0.1 y=-0.2 rot=0",
        "hikari: view model=hiyori_vts zoom=2.6 x=0.1 y=-0.2 rot=0",
        "hikari: view model=vtse2e1f zoom=1 x=0 y=0 rot=0",
        "hikari: view model=hiyori_vts zoom=1 x=0 y=0 rot=0",
    ])
    moving = [decode_rgb565(canned_frame(40 + 4 * (i % 12) + 40 * (i * 2 // FPS))) for i in range(FPS)]
    still = [decode_rgb565(canned_frame(40))] * FPS
    gusty = [decode_rgb565(canned_frame(40 + (40 if i // FPS % 2 else 4) * (i * 2 // FPS % 2))) for i in range(FPS * GUST_SECONDS)]
    steady = [decode_rgb565(canned_frame(40 + 20 * (i * 2 // FPS % 2))) for i in range(FPS * GUST_SECONDS)]
    cases = [
        ("lz4_match", (lz4_decode(bytes([0x24, 97, 98, 2, 0, 0]), 64) == b"ababababab", "overlap copy")),
        ("rgb565_decode", (moving[1][:3] == bytes((41, 44, 41)) and moving[1][-3:] == bytes(3), "first=%s last=%s" % (list(moving[1][:3]), list(moving[1][-3:])))),
        ("vts_copy_good", check_vts_copy(log)),
        ("vts_copy_remap_rejected", (not check_vts_copy(log + "\nhikari: vtube remap model=hiyori_vts ParamAngleX->ParamAngleZ")[0], "remap line must fail")),
        ("vts_copy_missing_rejected", (not check_vts_copy("")[0], "no line must fail")),
        ("plain_good", check_plain(log, "Haru", moving)),
        ("plain_silent_good", check_plain("", "Haru", still)),
        ("plain_params_rejected", (not check_plain("hikari: vtube model=Haru params=3 expressions=0 physics=50,0", "Haru", moving)[0], "nonzero params must fail")),
        ("plain_no_frames_rejected", (not check_plain(log, "Haru", [])[0], "no frames must fail")),
        ("remap_good", check_remap(log, "vtse2e1", moving, still)),
        ("remap_static_rejected", (not check_remap(log, "vtse2e1", still, still)[0], "static frames must fail")),
        ("remap_missing_rejected", (not check_remap(log.replace("ParamMouthOpenY->", "ParamA->"), "vtse2e1", moving, still)[0], "missing remap must fail")),
        ("wind_good", check_wind(log, "vtse2e1w100", "vtse2e1w0", moving * 2, still * 2)),
        ("wind_static_rejected", (not check_wind(log, "vtse2e1w100", "vtse2e1w0", still * 2, still * 2)[0], "static windy frames must fail")),
        ("wind_swapped_rejected", (not check_wind(log, "vtse2e1w0", "vtse2e1w100", moving * 2, still * 2)[0], "wrong physics line must fail")),
        ("muted_good", check_muted(log, "vtse2e1m0", moving * 2, still * 2, still * 2)),
        ("muted_moving_rejected", (not check_muted(log, "vtse2e1m0", moving * 2, moving * 2, still * 2)[0], "moving muted frames must fail")),
        ("gusts_good", check_gusts(gusty, still * GUST_SECONDS)),
        ("gusts_steady_rejected", (not check_gusts(steady, still * GUST_SECONDS)[0], "steady push must fail")),
        ("framing_good", check_framing(log, "vtse2e1f", FRAMED_VIEW, SAVED_VIEW)),
        ("framing_vts_copy_good", check_framing(log, VTS_COPY, VTS_VIEW, SAVED_VIEW)),
        ("framing_unimported_rejected", (not check_framing(log.replace("zoom=0.549 x=0.19281 y=0.48188 rot=-12.000001", "zoom=1 x=0 y=0 rot=0"), "vtse2e1f", FRAMED_VIEW, SAVED_VIEW)[0], "identity start must fail")),
        ("framing_saved_ignored_rejected", (not check_framing(log.replace("vtse2e1f zoom=2.6 x=0.1 y=-0.2", "vtse2e1f zoom=0.549 x=0.19281 y=0.48188"), "vtse2e1f", FRAMED_VIEW, SAVED_VIEW)[0], "saved framing must win")),
        ("framing_identity_overridden_rejected", (not check_framing(log.replace("hiyori_vts zoom=1 x=0 y=0 rot=0", "hiyori_vts zoom=0.36371255 x=-0.0000000000015118746 y=-0.055881098 rot=-0.0000000000000000000066410922"), VTS_COPY, VTS_VIEW, SAVED_VIEW)[0], "a client identity view must stay")),
        ("png_good", check_png(b"\x89PNG same", b"\x89PNG same")),
        ("png_diff_rejected", (not check_png(b"\x89PNG one", b"\x89PNG two")[0], "different bytes must fail")),
        ("jpeg_size", (jpeg_size(b"\xff\xd8\xff\xe0" + struct.pack(">H", 4) + b"ab\xff\xc0" + struct.pack(">HBHH", 11, 8, 120, 160) + b"\x03") == (160, 120), "app0 skipped, sof0 read")),
        ("jpeg_size_garbage", (jpeg_size(b"\xff\xd8junk") == (0, 0), "no sof gives 0x0")),
        ("cputime_seconds", (cputime_seconds("1:02.50") == 62.5 and cputime_seconds("1-01:00:00") == 90000.0, "m:ss.ss and d-hh:mm:ss")),
        ("mean_image", (mean_image([b"\x00\x04", b"\x02\x08"]) == [1.0, 6.0] and image_diff([1.0, 6.0], [2.0, 4.0]) == 1.5, "column mean and mean abs diff")),
    ]
    triangle = (4.0, [0.0, 0.0, 0, 1.0, 10.0, 0, 2.0, -10.0, 0, 4.0, 0.0])
    sawtooth = (4.0, [0.0, 0.0, 2, 1.0, 10.0, 0, 3.0, -10.0, 1, 3.3, -6.0, 3.6, -3.0, 4.0, 0.0])
    follow = [(int(-10 * curve_at(triangle[1], (0.7 + i / FPS) % triangle[0])), 225) for i in range(FPS * 5)]
    cases += [
        ("curve_bezier", (abs(curve_at(sawtooth[1], 3.3) + 6.643) < 0.001 and curve_at(sawtooth[1], 0.5) == 0.0, "bezier=%.3f stepped=%.3f" % (curve_at(sawtooth[1], 3.3), curve_at(sawtooth[1], 0.5)))),
        ("follows_good", check_follows(follow, [triangle], FOLLOW_WINDOW)),
        ("follows_flat_rejected", (not check_follows([(0, 225)] * FPS * 5, [triangle], FOLLOW_WINDOW)[0], "flat head must fail")),
        ("follows_other_curve_rejected", (not check_follows(follow, [sawtooth], FOLLOW_WINDOW)[0], "different curve must fail")),
    ]
    for name, outcome in cases:
        result.add(name, outcome)


def listen_pid(port):
    out = subprocess.run(["lsof", "-nP", "-t", "-iTCP:%d" % port, "-sTCP:LISTEN"], capture_output=True, text=True).stdout.split()
    return int(out[0]) if out else 0


def start_server(binary, port, log_path, args=(), model="", settings=None, files=None):
    holder = listen_pid(port)
    if holder:
        raise RuntimeError("port %d already in use by pid %d" % (port, holder))
    run_dir = tempfile.mkdtemp(prefix="hikari-e2e-")
    run_binary = shutil.copy2(binary, run_dir)
    for name, value in (files or {}).items():
        with open(os.path.join(run_dir, name), "w") as writer:
            json.dump(value, writer)
    with open(os.path.join(run_dir, "settings.json"), "w") as writer:
        json.dump(dict({"port": port, "model": model}, **(settings or {})), writer)
    log = open(log_path, "w")
    try:
        proc = subprocess.Popen([run_binary] + list(args), cwd=os.path.join(REPO, "native"), stdout=log, stderr=log, start_new_session=True)
    except BaseException:
        log.close()
        shutil.rmtree(run_dir, ignore_errors=True)
        raise
    handle = {"proc": proc, "log": log, "run_dir": run_dir}
    try:
        deadline = time.time() + TIMEOUT
        while time.time() < deadline:
            if proc.poll() is not None:
                raise RuntimeError("server exited with %s, log %s" % (proc.returncode, log_path))
            try:
                status = http_call(port, "GET", "/models")[0]
            except OSError:
                status = 0
            if status == 200:
                return handle
            time.sleep(0.5)
        raise RuntimeError("server never became ready, log %s" % log_path)
    except BaseException:
        stop_server(handle)
        raise


def stop_server(handle):
    proc = handle["proc"]
    if proc.poll() is None:
        os.killpg(os.getpgid(proc.pid), signal.SIGTERM)
        try:
            proc.wait(timeout=30)
        except subprocess.TimeoutExpired:
            os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
            proc.wait(timeout=10)
    handle["log"].close()
    shutil.rmtree(handle["run_dir"], ignore_errors=True)


def make_copy(target, name, source, vtube, still=False):
    shutil.copytree(os.path.join(SAMPLES, source), target, dirs_exist_ok=True)
    os.rename(os.path.join(target, source + ".model3.json"), os.path.join(target, name + ".model3.json"))
    if still:
        with open(os.path.join(target, name + ".model3.json")) as reader:
            model = json.load(reader)
        model["FileReferences"].pop("Motions", None)
        with open(os.path.join(target, name + ".model3.json"), "w") as writer:
            json.dump(model, writer)
    stray = [f for f in os.listdir(target) if f.endswith(".vtube.json")]
    if stray:
        raise RuntimeError("sample already has %s" % stray)
    with open(os.path.join(target, name + ".vtube.json"), "w") as handle:
        json.dump(vtube, handle)


def switch_model(port, name):
    status, body = http_call(port, "PUT", "/settings", json.dumps({"model": name}).encode())
    if status // 100 != 2:
        raise RuntimeError("switch to %s failed %d: %s" % (name, status, body.decode(errors="replace").strip()))


def idle_frames(port, count):
    status, frames, heads, _ = read_frames(port, "GET", "/listen?fps=%d&codec=rgb565&size=%dx%d" % (FPS, WIDTH, HEIGHT), count)
    return status, [decode_rgb565(frame) for frame in frames], heads


def render_images(port, pcm):
    status, frames, _, _ = read_frames(port, "POST", "/talk?fps=%d&codec=rgb565&size=%dx%d" % (FPS, WIDTH, HEIGHT), 400, wav(pcm))
    return status, [decode_rgb565(frame) for frame in frames]


def react_then_heads(port, model, kind, curves, want):
    switch_model(port, model)
    status, body = http_call(port, "POST", "/action?action=%s" % urllib.parse.quote(kind))
    played = json.loads(body) if status == 200 else {}
    _, _, heads = idle_frames(port, int((REACT_SKIP + REACT_SECONDS) * FPS))
    outcome = check_follows(heads[int(REACT_SKIP * FPS):], curves, REACT_SECONDS)
    return outcome[0] and played.get("motion") == want, "status=%d motion=%s %s" % (status, played.get("motion"), outcome[1])


def check_own_motions(port, result):
    animations = os.path.join(REPO, "models", VTS_COPY, "animations")
    with open(os.path.join(SAMPLES, PLAIN_MODEL, PLAIN_MODEL + ".model3.json")) as reader:
        groups = json.load(reader)["FileReferences"]["Motions"]
    curves = {group: [motion_curve(os.path.join(SAMPLES, PLAIN_MODEL, entry["File"]), FOLLOW_ID) for entry in entries] for group, entries in groups.items()}
    idle_checks = (
        ("10_vts_idle_animation", VTS_COPY, [motion_curve(os.path.join(animations, VTS_IDLE + ".motion3.json"), FOLLOW_ID)], "motion=%s " % VTS_IDLE),
        ("12_sample_idle_group", PLAIN_MODEL, curves["Idle"], ""),
    )
    for label, model, idle, extra in idle_checks:
        switch_model(port, model)
        status, _, heads = idle_frames(port, int((REACT_SKIP + 5) * FPS))
        outcome = check_follows(heads[int(REACT_SKIP * FPS):], idle, FOLLOW_WINDOW)
        result.add(label, (outcome[0] and status == 200, "model=%s %sstatus=%d %s" % (model, extra, status, outcome[1])))
    hotkey = motion_curve(os.path.join(animations, VTS_HOTKEY[1] + ".motion3.json"), FOLLOW_ID)
    result.add("11_vts_hotkey_animation", react_then_heads(port, VTS_COPY, VTS_HOTKEY[0], [hotkey], VTS_HOTKEY[0]))
    result.add("13_sample_pet_tapbody", react_then_heads(port, PLAIN_MODEL, "pet", curves["TapBody"], "TapBody"))


def check_switch_stream(port, first, second):
    switch_model(port, first)
    path = "/listen?fps=%d&codec=rgb565&size=%dx%d" % (FPS, WIDTH, HEIGHT)
    count = FPS * SWITCH_SECONDS * 3
    with concurrent.futures.ThreadPoolExecutor(1) as pool:
        stream = pool.submit(read_frames, port, "GET", path, count)
        time.sleep(SWITCH_SECONDS)
        asked = time.monotonic()
        switch_model(port, second)
        switched = time.monotonic()
        status, frames, _, times = stream.result()
    _, body = http_call(port, "GET", "/models")
    current = json.loads(body).get("model")
    early = [decode_rgb565(frame) for frame, moment in zip(frames, times) if asked - 2 <= moment < asked - 1]
    late = [decode_rgb565(frame) for frame, moment in zip(frames, times) if asked - 1 <= moment < asked]
    after = [decode_rgb565(frame) for frame, moment in zip(frames, times) if moment >= switched + SWITCH_SETTLE]
    drift = image_diff(mean_image(early), mean_image(late))
    change = image_diff(mean_image(late), mean_image(after))
    ok = status == 200 and len(frames) == count and len(after) >= FPS and current == second and change >= max(SWITCH_FLOOR, SWITCH_RATIO * drift)
    return ok, "%s->%s status=%d frames=%d/%d after_switch=%d switch_took=%.2fs models.model=%s change=%.2f drift=%.2f need>=max(%.1f,%.1fx drift)" % (first, second, status, len(frames), count, len(after), switched - asked, current, change, drift, SWITCH_FLOOR, SWITCH_RATIO)


def check_shared_heads(port):
    with concurrent.futures.ThreadPoolExecutor(2) as pool:
        big = pool.submit(read_frames, port, "GET", "/listen?fps=%d&codec=jpeg&size=960x720" % FPS, FPS * (SHARED_SECONDS + 1))
        time.sleep(1.0)
        small = pool.submit(read_frames, port, "GET", "/listen?fps=%d&codec=pal256&size=160x120" % FPS, FPS * SHARED_SECONDS)
        big_status, _, big_heads, big_times = big.result()
        small_status, _, small_heads, small_times = small.result()
    gaps = []
    for head, moment in zip(small_heads, small_times):
        index = bisect.bisect_left(big_times, moment)
        near = [(abs(big_times[i] - moment), i) for i in (index - 1, index) if 0 <= i < len(big_times)]
        if near and min(near)[0] <= 1.0 / FPS:
            other = big_heads[min(near)[1]]
            gaps.append(abs(head[0] - other[0]) + abs(head[1] - other[1]))
    yaws = [yaw for yaw, _ in small_heads]
    spread = max(yaws) - min(yaws) if yaws else 0
    share = sum(gap <= HEAD_TOLERANCE for gap in gaps) / len(gaps) if gaps else 0.0
    ok = big_status == 200 and small_status == 200 and len(gaps) >= FPS * SHARED_SECONDS // 2 and spread >= SHARED_RANGE and share >= HEAD_SHARE
    return ok, "status=%d/%d matched=%d yaw_spread=%d need>=%d gap_median=%s gap_max=%s within_%d=%.2f need>=%.2f" % (big_status, small_status, len(gaps), spread, SHARED_RANGE, statistics.median(gaps) if gaps else None, max(gaps) if gaps else None, HEAD_TOLERANCE, share, HEAD_SHARE)


def check_render_mouth(port, model):
    switch_model(port, model)
    path = "/listen?fps=%d&codec=rgb565&size=%dx%d" % (FPS, WIDTH, HEIGHT)
    with concurrent.futures.ThreadPoolExecutor(1) as pool:
        stream = pool.submit(read_frames, port, "GET", path, FPS * (PCM_SECONDS + 3))
        time.sleep(1.5)
        started = time.monotonic()
        render_status, rendered = render_images(port, synth_pcm())
        status, frames, _, times = stream.result()
    before = [decode_rgb565(frame) for frame, moment in zip(frames, times) if moment < started]
    during = [decode_rgb565(frame) for frame, moment in zip(frames, times) if started + 0.2 <= moment < started + PCM_SECONDS]
    quiet = mouth_spread(before)
    talking = mouth_spread(during)
    ok = status == 200 and render_status == 200 and len(before) >= FPS and len(during) >= FPS and talking > quiet + MOTION_MARGIN
    return ok, "model=%s status=%d render=%d/%d frames before=%d during=%d mouth_spread before=%.3f during=%.3f margin=%.2f" % (model, status, render_status, len(rendered), len(before), len(during), quiet, talking, MOTION_MARGIN)


def check_sizes(port):
    body = wav(synth_pcm()[:4800])
    rejects = [
        ("GET", "/listen?fps=%d&codec=jpeg&size=0x10" % FPS, None, SIZE_ERROR % "0x10"),
        ("GET", "/listen?fps=%d&codec=jpeg&size=abc" % FPS, None, SIZE_ERROR % "abc"),
        ("GET", "/listen?fps=%d&codec=jpeg&size=3841x10" % FPS, None, SIZE_ERROR % "3841x10"),
        ("GET", "/listen?fps=%d&codec=jpeg&size=160x120&size=160x120" % FPS, None, "hikari: size given more than once"),
        ("POST", "/talk?fps=%d&codec=jpeg&size=abc" % FPS, body, SIZE_ERROR % "abc"),
    ]
    bad = []
    for method, path, payload, line in rejects:
        status, got = http_call(port, method, path, payload)
        if status != 400 or got.decode(errors="replace").strip() != line:
            bad.append((path, status, got[:120]))
    streams = [
        ("/listen?fps=%d&codec=jpeg&size=160x120" % FPS, (160, 120)),
        ("/listen?fps=%d&codec=jpeg" % FPS, DEFAULT_SIZE),
        ("/listen?fps=%d&codec=jpeg&robot=true" % FPS, DEFAULT_SIZE),
    ]
    sizes = []
    for path, want in streams:
        status, frames, _, _ = read_frames(port, "GET", path, 2)
        got = jpeg_size(frames[0]) if frames else (0, 0)
        sizes.append((path, status, got))
        if status != 200 or got != want:
            bad.append((path, status, got))
    return not bad, "rejects=%d streams=%s bad=%s" % (len(rejects), sizes, bad)


def listener_load(port, pid, listeners):
    path = "/listen?fps=%d&codec=jpeg&size=960x720" % FPS
    with concurrent.futures.ThreadPoolExecutor(listeners) as pool:
        streams = [pool.submit(read_frames, port, "GET", path, FPS * (CPU_SECONDS + 3)) for _ in range(listeners)]
        time.sleep(1.5)
        _, start_cpu = process_usage(pid)
        started = time.monotonic()
        time.sleep(CPU_SECONDS)
        rss, cpu = process_usage(pid)
        ended = time.monotonic()
        outcomes = [stream.result() for stream in streams]
    frames = sum(1 for moment in outcomes[0][3] if started <= moment < ended)
    return (cpu - start_cpu) * 1000 / max(frames, 1), rss / 1024, frames, [outcome[0] for outcome in outcomes]


def check_cpu(port, pid):
    one = listener_load(port, pid, 1)
    three = listener_load(port, pid, 3)
    ratio = three[0] / one[0] if one[0] else math.inf
    ok = all(status == 200 for status in one[3] + three[3]) and one[2] >= FPS and three[2] >= FPS and ratio < CPU_RATIO
    return ok, "cpu_ms_per_frame 1=%.2f 3=%.2f ratio=%.2f need<%.1f rss_mb 1=%.1f 3=%.1f frames=%d/%d over %ds statuses=%s/%s" % (one[0], three[0], ratio, CPU_RATIO, one[1], three[1], one[2], three[2], CPU_SECONDS, one[3], three[3])


def run(args, result):
    name = "vtse2e%d" % os.getpid()
    winds = {"%sw%d" % (name, wind): wind for wind in (100, 0)}
    muted = name + "m0"
    framed = name + "f"
    copies = [os.path.join(REPO, "models", model) for model in [name, muted, framed] + list(winds)]
    log_path = args.out + ".log"
    handle = None
    for copy in copies:
        os.mkdir(copy)
    for sample in (PLAIN_MODEL, WIND_MODEL, SOURCE_MODEL):
        sample_copy = os.path.join(REPO, "models", sample)
        if not os.path.exists(sample_copy):
            copies.append(sample_copy)
            shutil.copytree(os.path.join(SAMPLES, sample), sample_copy)
    vts_copy = os.path.join(REPO, "models", VTS_COPY)
    if not os.path.exists(vts_copy):
        copies.append(vts_copy)
        shutil.copytree(VTS_SOURCE, vts_copy)
        for suffix in (".model3.json", ".vtube.json"):
            os.rename(os.path.join(vts_copy, "hiyori" + suffix), os.path.join(vts_copy, VTS_COPY + suffix))
    try:
        make_copy(copies[0], name, SOURCE_MODEL, {"ParameterSettings": [{"Name": output, "Input": vts, "OutputLive2D": output} for vts, output in ROUTES]})
        for model, wind in winds.items():
            make_copy(os.path.join(REPO, "models", model), model, WIND_MODEL, {"PhysicsSettings": {"Use": True, "PhysicsStrength": 50, "WindStrength": wind}}, still=True)
        with open(os.path.join(SAMPLES, WIND_MODEL, WIND_MODEL + ".physics3.json")) as reader:
            groups = [group["Id"] for group in json.load(reader)["PhysicsSettings"]]
        make_copy(copies[1], muted, WIND_MODEL, {"PhysicsSettings": {"Use": True, "PhysicsStrength": 50, "WindStrength": 100}, "PhysicsCustomizationSettings": {"PhysicsMultipliersPerPhysicsGroup": [{"ID": group, "Value": 0} for group in groups]}}, still=True)
        make_copy(copies[2], framed, PLAIN_MODEL, {"SavedModelPosition": FRAMED_POSITION})
        handle = start_server(args.binary, args.port, log_path, model=VTS_COPY)
        vts_status, _, _ = idle_frames(args.port, 2)
        view_statuses = [http_call(args.port, "PUT", "/view?zoom=%g&x=%g&y=%g&rot=%g" % view)[0] for view in (SAVED_VIEW, IDENTITY_VIEW)]
        switch_model(args.port, framed)
        framed_status, _, _ = idle_frames(args.port, 2)
        view_statuses += [http_call(args.port, "PUT", "/view?zoom=%g&x=%g&y=%g&rot=%g" % view)[0] for view in (SAVED_VIEW, IDENTITY_VIEW)]
        with open(log_path) as reader:
            log = reader.read()
        outcome = check_vts_copy(log)
        result.add("1_vts_copy", (outcome[0] and vts_status == 200, "status=%d %s" % (vts_status, outcome[1])))
        for label, model, start in (("8_framing_copy", framed, FRAMED_VIEW), ("9_framing_vts_copy", VTS_COPY, VTS_VIEW)):
            outcome = check_framing(log, model, start, SAVED_VIEW)
            result.add(label, (outcome[0] and framed_status == 200 and view_statuses == [204] * 4, "status=%d views=%s %s" % (framed_status, view_statuses, outcome[1])))
        switch_model(args.port, PLAIN_MODEL)
        status, images, _ = idle_frames(args.port, FPS * 2)
        with open(log_path) as reader:
            outcome = check_plain(reader.read(), PLAIN_MODEL, images)
        result.add("2_plain_sample", (outcome[0] and status == 200, "status=%d %s" % (status, outcome[1])))
        pcm = synth_pcm()
        switch_model(args.port, name)
        copy_status, copy_images = render_images(args.port, pcm)
        switch_model(args.port, SOURCE_MODEL)
        source_status, source_images = render_images(args.port, pcm)
        with open(log_path) as reader:
            outcome = check_remap(reader.read(), name, copy_images, source_images)
        result.add("3_remap", (outcome[0] and copy_status == 200 and source_status == 200, "model=%s status=%d/%d frames=%d/%d %s" % (name, copy_status, source_status, len(copy_images), len(source_images), outcome[1])))
        windy, calm = list(winds)
        switch_model(args.port, windy)
        windy_status, windy_images, _ = idle_frames(args.port, FPS * GUST_SECONDS)
        switch_model(args.port, calm)
        calm_status, calm_images, _ = idle_frames(args.port, FPS * GUST_SECONDS)
        switch_model(args.port, muted)
        muted_status, muted_images, _ = idle_frames(args.port, FPS * 3)
        with open(log_path) as reader:
            outcome = check_wind(reader.read(), windy, calm, windy_images, calm_images)
        result.add("5_wind", (outcome[0] and windy_status == 200 and calm_status == 200, "status=%d/%d frames=%d/%d %s" % (windy_status, calm_status, len(windy_images), len(calm_images), outcome[1])))
        with open(log_path) as reader:
            outcome = check_muted(reader.read(), muted, windy_images, muted_images, calm_images)
        result.add("6_wind_muted", (outcome[0] and muted_status == 200, "model=%s groups=%d status=%d frames=%d %s" % (muted, len(groups), muted_status, len(muted_images), outcome[1])))
        result.add("7_gusts", check_gusts(windy_images, calm_images))
        check_own_motions(args.port, result)
        result.add("14_switch_keeps_stream", check_switch_stream(args.port, PLAIN_MODEL, SOURCE_MODEL))
        switch_model(args.port, PLAIN_MODEL)
        result.add("15_shared_heads", check_shared_heads(args.port))
        result.add("16_render_drives_idle_mouth", check_render_mouth(args.port, name))
        status, _ = http_call(args.port, "GET", "/mirror")
        result.add("17_mirror_gone", (status == 404, "status=%d" % status))
        result.add("18_sizes", check_sizes(args.port))
        switch_model(args.port, PLAIN_MODEL)
        result.add("19_cpu_per_listener", check_cpu(args.port, handle["proc"].pid))
    finally:
        try:
            if handle:
                stop_server(handle)
        finally:
            for copy in copies:
                shutil.rmtree(copy, ignore_errors=True)
    with open(args.selftest_png, "rb") as first, open(args.main_png, "rb") as second:
        result.add("4_selftest_png", check_png(first.read(), second.read()))


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary")
    parser.add_argument("--out")
    parser.add_argument("--port", type=int, default=PORT)
    parser.add_argument("--selftest-png", default=os.path.join(REPO, "native/engine/build/selftest.png"))
    parser.add_argument("--main-png")
    parser.add_argument("--selftest", action="store_true")
    args = parser.parse_args()
    if not args.selftest and not (args.binary and args.out and args.main_png):
        parser.error("--binary, --out and --main-png are required without --selftest")
    if args.port == LIVE_PORT:
        parser.error("port %d is the live service" % LIVE_PORT)
    result = Result(args.out)
    try:
        if args.selftest:
            selftest(result)
        else:
            run(args, result)
    except BaseException as error:
        result.payload["error"] = "%s: %s" % (type(error).__name__, error)
        result.flush()
        raise
    return finish(result)


if __name__ == "__main__":
    sys.exit(main())
