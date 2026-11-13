#!/usr/bin/env python3
import argparse
import functools
import json
import math
import os
import re
import shutil
import statistics
import struct
import subprocess
import tempfile
import sys
import threading
import time
import zlib

import vts_e2e

PORT = 9321
MODEL = "statehiyori%d" % os.getpid()
SPEAKER = "statemao%d" % os.getpid()
HARU = "stateharu%d" % os.getpid()
OVERRIDE = "stateover%d" % os.getpid()
FPS = vts_e2e.FPS
WIDTH = vts_e2e.WIDTH
SETTLE_SECONDS = 3.0
HOLD_SECONDS = 2.0
MEAN_SECONDS = 1.0
CYCLES = 6
EYES = ((60, 78), (58, 102))
MOUTH = ((76, 90), (68, 92))
SMIRK = {"Type": "Live2D Expression", "FadeInTime": 0.3, "FadeOutTime": 0.3, "Parameters": [{"Id": "ParamMouthForm", "Value": 1, "Blend": "Overwrite"}, {"Id": "ParamMouthOpenY", "Value": 0.5, "Blend": "Overwrite"}]}
SMIRK_VIEW = "/view?zoom=4&x=0&y=0.9"
SMIRK_BOX = ((42, 58), (66, 96))
BLUSH = {"Version": 3, "Meta": {"Duration": 3.0, "Fps": 30.0, "FadeInTime": 0.3, "FadeOutTime": 0.5, "Loop": False, "AreBeziersRestricted": False, "CurveCount": 1, "TotalSegmentCount": 1, "TotalPointCount": 2, "UserDataCount": 0, "TotalUserDataSize": 0}, "Curves": [{"Target": "Parameter", "Id": "ParamCheek", "Segments": [0.0, 1.0, 0, 3.0, 1.0]}]}
BLUSH_BOX = ((10, 38), (15, 50))
REST_RATIO = 2.0
CHANGE_RATIO = 3.0
CHANGE_FLOOR = 1.0
ACTION_EVERY = 1.0
QUIET_EVERY = 1000.0
ACTION_LINE = re.compile(r"hikari: idle random action=(\S+)")
START = {"face": "neutral", "pose": "none", "asleep": False}
BLINKS = ["double_blink"]
BASIC = ["look_around", "glance_left", "glance_right", "double_blink", "yawn", "tilt_smile", "look_up_think"]
DEFAULTS = ["look_left", "look_right", "look_up", "look_down", "look_ahead", "dance"] + BASIC
BLINK_SECONDS = 2.0
CLOSE_RISE = 8.0
CLOSE_SPAN = 5
BLINK_CLOSES = 2
BLINK_CYCLES = 3
EVERY_RANGE = [1, 3]
EVERY_GAPS = 6
ACTION_SPAN = 4
ACTION_CYCLES = 4
SLEEP_SECONDS = 5.0
WAKE_SECONDS = 1.5
SLEEP_FLOOR = 1.0
GAP_LIMIT = 2.0 / FPS
LOOK_YAW = 150
LOOK_PITCH = 150
AHEAD_BAND = 30
PITCH_HOME = 150
LOOKS = {"look_left": (0, -1), "look_right": (0, 1), "look_up": (1, 1), "look_down": (1, -1)}
GIF = b"GIF89a\x01\x00\x01\x00\x80\x00\x00\x00\x00\x00\xff\xff\xff!\xf9\x04\x01\x00\x00\x00\x00,\x00\x00\x00\x00\x01\x00\x01\x00\x00\x02\x02D\x01\x00;"


def crop(image):
    return [value for (top, bottom), (left, right) in (EYES, MOUTH) for y in range(top, bottom) for value in image[(y * WIDTH + left) * 3:(y * WIDTH + right) * 3]]


def region_crop(box, image):
    (top, bottom), (left, right) = box
    return [value for y in range(top, bottom) for value in image[(y * WIDTH + left) * 3:(y * WIDTH + right) * 3]]


def state_call(port, method, query):
    status, body = vts_e2e.http_call(port, method, "/state?" + query)
    return status, body.decode(errors="replace").strip()


def settings_call(port, values):
    status, body = vts_e2e.http_call(port, "PUT", "/settings", json.dumps(values).encode())
    return status, body.decode(errors="replace").strip()


def current_settings(port):
    status, body = vts_e2e.http_call(port, "GET", "/settings")
    if status != 200:
        raise RuntimeError("GET /settings returned %d" % status)
    return json.loads(body)


def timed_stream(port, posts, call=None):
    count = int((SETTLE_SECONDS + HOLD_SECONDS * (len(posts) or CYCLES * 2) + MEAN_SECONDS) * FPS)
    timers = [threading.Timer(SETTLE_SECONDS + HOLD_SECONDS * index, call or state_call, (port, "POST", query)) for index, query in enumerate(posts)]
    for timer in timers:
        timer.start()
    status, images, heads = vts_e2e.idle_frames(port, count)
    for timer in timers:
        timer.join()
    return status, images, heads


def window(images, index):
    end = int((SETTLE_SECONDS + HOLD_SECONDS * index) * FPS)
    return images[end - int(MEAN_SECONDS * FPS):end]


def phase_totals(images, cut):
    means = [[statistics.median(column) for column in zip(*[cut(image) for image in window(images, index)])] for index in range(CYCLES * 2 + 1)]
    total = [0.0] * len(means[0])
    for index in range(1, CYCLES * 2, 2):
        for pixel, (before, on, after) in enumerate(zip(means[index - 1], means[index], means[index + 1])):
            total[pixel] += on - (before + after) / 2
    return total


def phase_change(images, cut=crop):
    total = phase_totals(images, cut)
    return sum(abs(value) for value in total) / len(total) / CYCLES


def cycled_change(port, posts, cut=crop, call=None):
    status, changed, _ = timed_stream(port, posts * CYCLES, call)
    control_status, control, _ = timed_stream(port, [], call)
    change = phase_change(changed, cut)
    drift = phase_change(control, cut)
    ratio = change / drift if drift else math.inf
    ok = status == 200 and control_status == 200 and len(changed) == len(control) and change >= CHANGE_FLOOR and ratio >= CHANGE_RATIO
    return ok, "status=%d/%d frames=%d/%d change=%.3f drift=%.3f ratio=%.2f need change>=%.1f ratio>=%.1f (%d cycles of %s, %.1fs mean windows, settle %.1fs, drift is the same windows on a no-POST control)" % (status, control_status, len(changed), len(control), change, drift, ratio, CHANGE_FLOOR, CHANGE_RATIO, CYCLES, posts, MEAN_SECONDS, SETTLE_SECONDS)


def check_change(port, model, posts):
    vts_e2e.switch_model(port, model)
    ok, detail = cycled_change(port, posts)
    return ok, "model=%s crops=%s+%s %s" % (model, EYES, MOUTH, detail)


def action_call(port, name):
    status, body = vts_e2e.http_call(port, "POST", "/action?action=%s" % name)
    return status, body.decode(errors="replace").strip()


def action_post(port, method, name):
    if name:
        action_call(port, name)


def check_action_now(port, model, name):
    vts_e2e.switch_model(port, model)
    ok, detail = cycled_change(port, [name, ""], crop, action_post)
    return ok, "model=%s action=%s %s" % (model, name, detail)


def check_action_contract(port):
    react_status, _ = vts_e2e.http_call(port, "POST", "/react?kind=pet_end")
    old_param = vts_e2e.http_call(port, "POST", "/action?name=look_left")[0]
    unknown = action_call(port, "nosuch")
    played = action_call(port, "look_left")
    body = json.loads(played[1]) if played[0] == 200 else {}
    ok = react_status == 404 and old_param == 400 and unknown == (400, "hikari: unknown action nosuch") and played[0] == 200 and sorted(body) == ["action", "expression", "motion"] and body["action"] == "look_left"
    return ok, "react=%d name_param=%d unknown=%s look_left=%s" % (react_status, old_param, unknown, played)


def check_get_actions(port):
    status, body = vts_e2e.http_call(port, "GET", "/action")
    listed = json.loads(body) if status == 200 else {}
    missing = [name for name in DEFAULTS + ["sleep"] if name not in listed.get("actions", [])]
    idle = current_settings(port)["idle_actions"]
    return status == 200 and not missing and listed.get("idle") == idle, "status=%d actions=%d missing=%s idle=%s settings_idle=%s" % (status, len(listed.get("actions", [])), missing, listed.get("idle"), idle)


def crop_at(images, index):
    return [statistics.median(column) for column in zip(*[crop(image) for image in window(images, index)])]


def crop_distance(first, second):
    return sum(abs(a - b) for a, b in zip(first, second)) / len(first)


def action_shift(port, name):
    images = timed_stream(port, [name], action_post)[1]
    return crop_distance(crop_at(images, 0), crop_at(images, 1)), crop_at(images, 1)


def onset_change(images, shift):
    total = None
    for cycle in range(ACTION_CYCLES):
        before, on = ([statistics.median(column) for column in zip(*[crop(image) for image in window(images, ACTION_SPAN * cycle + shift + index)])] for index in (0, 1))
        delta = [b - a for a, b in zip(before, on)]
        total = delta if total is None else [t + d for t, d in zip(total, delta)]
    return sum(abs(value) for value in total) / len(total) / ACTION_CYCLES


def close_counts(images, shift):
    counts = []
    for cycle in range(ACTION_CYCLES):
        onset = ACTION_SPAN * cycle + shift
        before = [statistics.median(column) for column in zip(*[region_crop(EYES, image) for image in window(images, onset)])]
        start = int((SETTLE_SECONDS + HOLD_SECONDS * onset) * FPS)
        distances = [crop_distance(before, region_crop(EYES, image)) for image in images[start:start + int(MEAN_SECONDS * FPS) + CLOSE_SPAN]]
        closes = 0
        for index in range(1, len(distances)):
            value = distances[index]
            left = distances[max(0, index - CLOSE_SPAN):index + 1]
            right = distances[index:index + CLOSE_SPAN + 1]
            if distances[index - 1] < value and value == max(left + right) and value - min(left) >= CLOSE_RISE and value - min(right) >= CLOSE_RISE:
                closes += 1
        counts.append(closes)
    return counts


def head_at(heads, index, axis):
    values = [head[axis] for head in window(heads, index)]
    return statistics.median(values)


def look_poses(heads, shift):
    return [tuple(head_at(heads, ACTION_SPAN * cycle + shift + index, axis) for index in (0, 1) for axis in (0, 1)) for cycle in range(ACTION_CYCLES)]


def look_ok(name, pose):
    if name == "look_ahead":
        return abs(pose[0]) >= LOOK_YAW and abs(pose[2]) <= AHEAD_BAND and abs(pose[3] - PITCH_HOME) <= AHEAD_BAND
    axis, sign = LOOKS[name]
    return (pose[2 + axis] - pose[axis]) * sign >= (LOOK_YAW, LOOK_PITCH)[axis]


def check_default_actions(port):
    vts_e2e.switch_model(port, MODEL)
    settings_call(port, {"idle_actions": [], "action_every": QUIET_EVERY})
    quiet = [""] * ACTION_SPAN * ACTION_CYCLES
    control = timed_stream(port, quiet, action_post)[1]
    drift = max(onset_change(control, 0), onset_change(control, 1))
    floor = max(drift * CHANGE_RATIO, CHANGE_FLOOR)
    control_closes = close_counts(control, 0)
    changes = {}
    closes = {}
    poses = {}
    for name in DEFAULTS:
        lead = ["look_left"] if name == "look_ahead" else ["look_ahead"] if name in LOOKS else []
        posts = (lead + [name] + [""] * (ACTION_SPAN - 1 - len(lead))) * ACTION_CYCLES
        _, images, heads = timed_stream(port, posts, action_post)
        if name in LOOKS or name == "look_ahead":
            poses[name] = look_poses(heads, len(lead))
        elif name in BLINKS:
            closes[name] = close_counts(images, len(lead))
        else:
            changes[name] = onset_change(images, len(lead))
    blinked = {name: sum(count >= BLINK_CLOSES for count in counts) >= BLINK_CYCLES for name, counts in dict(closes, control=control_closes).items()}
    flat = [name for name, change in changes.items() if change < floor] + [name for name, rows in poses.items() if not all(look_ok(name, pose) for pose in rows)] + [name for name in closes if not blinked[name]]
    return not flat and not blinked["control"], "drift=%.3f floor=%.3f flat=%s changes=%s eye_closes_per_cycle=%s control_closes=%s need >=%d closes in >=%d cycles within %.1fs of onset, a close being an eye-crop distance peak rising and falling >=%.1f within %d frames, control must not pass; head(yaw,pitch before->after)=%s need yaw>=%d pitch>=%d ahead within %d of (0,%d) (%d onsets each, next onset %ds later)" % (drift, floor, flat, {name: round(change, 2) for name, change in changes.items()}, closes, control_closes, BLINK_CLOSES, BLINK_CYCLES, MEAN_SECONDS, CLOSE_RISE, CLOSE_SPAN, {name: ["%d,%d->%d,%d" % pose for pose in rows] for name, rows in poses.items()}, LOOK_YAW, LOOK_PITCH, AHEAD_BAND, PITCH_HOME, ACTION_CYCLES, ACTION_SPAN * HOLD_SECONDS)


def check_model_overrides_default(port, name):
    played = action_call(port, "dance")
    vts_e2e.switch_model(port, name)
    over = action_shift(port, "dance")[1]
    look = action_shift(port, "look_left")[1]
    vts_e2e.switch_model(port, MODEL)
    plain = action_shift(port, "dance")[1]
    same, apart = crop_distance(over, look), crop_distance(over, plain)
    return same * CHANGE_RATIO < apart, "override_dance_vs_look_left=%.3f override_dance_vs_default_dance=%.3f first=%s" % (same, apart, played)


def check_trimmed_defaults(args, trimmed):
    log_path = args.out + ".trimmed.log"
    handle = vts_e2e.start_server(args.binary, args.port, log_path, model=MODEL, files={"default.model3.json": trimmed})
    try:
        dance = action_call(args.port, "dance")
        yawn = action_call(args.port, "yawn")
    finally:
        vts_e2e.stop_server(handle)
    return dance == (400, "hikari: unknown action dance") and yawn[0] == 200, "dance=%s yawn=%s" % (dance, yawn)


def check_every_range(port, log_path):
    bad = {json.dumps(value): settings_call(port, {"action_every": value}) for value in [0, -1, 1.5, [3, 1], [0, 2], [1], [1, 2, 3], "5", [1, 2.5]]}
    rejected = all(status == 400 and body == "hikari: action_every must be whole seconds above 0 or [min, max], got %s" % key.replace(" ", "") for key, (status, body) in bad.items())
    vts_e2e.switch_model(port, MODEL)
    put = settings_call(port, {"idle_actions": ["double_blink"], "action_every": EVERY_RANGE})
    echoed = current_settings(port)["action_every"]
    mark = os.path.getsize(log_path)
    stream = threading.Thread(target=vts_e2e.idle_frames, args=(port, int((BLINK_SECONDS + EVERY_RANGE[1]) * (EVERY_GAPS + 1) * FPS)))
    stream.start()
    started = []
    seen = 0
    while stream.is_alive():
        count = len(log_actions(log_path, mark))
        started += [time.monotonic()] * (count - seen)
        seen = count
        time.sleep(0.01)
    stream.join()
    settings_call(port, {"idle_actions": [], "action_every": QUIET_EVERY})
    gaps = [later - earlier - BLINK_SECONDS for earlier, later in zip(started, started[1:])]
    slack = 2.0 / FPS + 0.02
    inside = all(EVERY_RANGE[0] - slack <= gap <= EVERY_RANGE[1] + slack for gap in gaps)
    return rejected and put[0] == 200 and echoed == EVERY_RANGE and len(gaps) >= EVERY_GAPS - 1 and inside, "bad=%s put=%s echoed=%s gaps=%s" % ({key: status for key, (status, _) in bad.items()}, put[0], echoed, [round(gap, 3) for gap in gaps])


def check_idle_dance(port, log_path):
    mark = os.path.getsize(log_path)
    put = settings_call(port, {"idle_actions": ["dance"], "action_every": ACTION_EVERY})
    vts_e2e.idle_frames(port, int(ACTION_EVERY * 6 * FPS))
    started = log_actions(log_path, mark)
    settings_call(port, {"idle_actions": BASIC, "action_every": QUIET_EVERY})
    return put[0] == 200 and "dance" in started and set(started) == {"dance"}, "put=%s started=%s" % (put, started)


def check_idle_sleep_rejected(port, log_path):
    put = settings_call(port, {"idle_actions": ["yawn", "sleep"]})
    with open(log_path) as reader:
        startup = "hikari: sleep can't be an idle action" in reader.read()
    idle = current_settings(port)["idle_actions"]
    return put == (400, "hikari: sleep can't be an idle action") and startup and "sleep" not in idle, "put=%s startup_line=%s idle=%s" % (put, startup, idle)


def mean_crop(images):
    return vts_e2e.mean_image([crop(image) for image in images])


def check_sleep_action(port, log_path):
    settings_call(port, {"idle_actions": [], "action_every": QUIET_EVERY})
    awake = vts_e2e.idle_frames(port, int((SETTLE_SECONDS + MEAN_SECONDS) * FPS))[1][-int(MEAN_SECONDS * FPS):]
    action_call(port, "sleep")
    settings_call(port, {"idle_actions": BASIC, "action_every": ACTION_EVERY})
    mark = os.path.getsize(log_path)
    asleep = vts_e2e.idle_frames(port, int((1 + SLEEP_SECONDS) * FPS))[1]
    during = log_actions(log_path, mark)
    _, state = state_call(port, "GET", "")
    settings_call(port, {"idle_actions": [], "action_every": QUIET_EVERY})
    woke_at = time.monotonic()
    action_call(port, "look_left")
    after = vts_e2e.idle_frames(port, int(WAKE_SECONDS * FPS))[1]
    _, woke = state_call(port, "GET", "")
    closed = mean_crop(asleep[FPS:2 * FPS])
    held = vts_e2e.image_diff(closed, mean_crop(asleep[-FPS:]))
    fell = vts_e2e.image_diff(mean_crop(awake), closed)
    woken = vts_e2e.image_diff(closed, mean_crop(after[-FPS // 2:]))
    ok = fell >= SLEEP_FLOOR and held < fell / CHANGE_RATIO and not during and json.loads(state).get("asleep") is True and json.loads(woke).get("asleep") is False and woken >= SLEEP_FLOOR
    return ok, "awake_vs_asleep=%.3f asleep_start_vs_end=%.3f (5s, action_every=1) idle_actions_while_asleep=%s state=%s look_left_vs_asleep=%.3f within %.1fs woke=%s took=%.2fs" % (fell, held, during, state, woken, WAKE_SECONDS, woke, time.monotonic() - woke_at)


def check_smirk(port):
    vts_e2e.switch_model(port, MODEL)
    _, listed = vts_e2e.http_call(port, "GET", "/state")
    listed = "smirk" in json.loads(listed)["states"]
    vts_e2e.http_call(port, "PUT", SMIRK_VIEW)
    ok, detail = cycled_change(port, ["face=smirk", "face=neutral"], functools.partial(region_crop, SMIRK_BOX))
    vts_e2e.http_call(port, "PUT", "/view?zoom=1&x=0&y=0")
    return ok and listed, "listed=%s view=%s region=%s %s" % (listed, SMIRK_VIEW, SMIRK_BOX, detail)


def check_action_params_rest(port, name):
    vts_e2e.switch_model(port, name)
    vts_e2e.http_call(port, "PUT", SMIRK_VIEW)
    status, images, _ = timed_stream(port, ["blush", ""], action_post)
    vts_e2e.http_call(port, "PUT", "/view?zoom=1&x=0&y=0")
    before, on, after = ([statistics.median(column) for column in zip(*[region_crop(BLUSH_BOX, image) for image in window(images, index)])] for index in range(3))
    blush, rest, held = crop_distance(before, on), crop_distance(before, after), crop_distance(on, after)
    ok = status == 200 and blush >= CHANGE_FLOOR and held >= REST_RATIO * rest
    return ok, "status=%d blush=%.3f rest=%.3f held=%.3f need blush>=%.1f and held>=%.1fx rest (rest=before vs after, held=during vs after, ParamCheek motion 3.0s fade-out 0.5s at %.1fs, rest window %.1f-%.1fs, view=%s region=%s)" % (status, blush, rest, held, CHANGE_FLOOR, REST_RATIO, SETTLE_SECONDS, SETTLE_SECONDS + 2 * HOLD_SECONDS - MEAN_SECONDS, SETTLE_SECONDS + 2 * HOLD_SECONDS, SMIRK_VIEW, BLUSH_BOX)


def check_tapbody(port, name):
    vts_e2e.switch_model(port, name)
    played = action_call(port, "TapBody")
    ok, detail = cycled_change(port, ["TapBody", ""], list, action_post)
    return ok and json.loads(played[1]).get("motion") == "TapBody", "first=%s %s" % (played, detail)


def check_idle_loop(args, name, loop):
    log_path = args.out + ".loop.log"
    handle = vts_e2e.start_server(args.binary, args.port + 1, log_path, model=name, settings={"idle_loop": loop, "face": "nosuch"})
    try:
        settings = current_settings(args.port + 1)
        _, state = state_call(args.port + 1, "GET", "")
    finally:
        vts_e2e.stop_server(handle)
    with open(log_path) as reader:
        log = reader.read()
    ok = settings.get("idle_loop") == loop and ("hikari: idle loop %s" % loop) in log and "settings.json face \"nosuch\" is not a state" in log and json.loads(state)["face"] == "neutral"
    return ok, "settings.idle_loop=%s loop_line=%s face_line=%s state=%s" % (settings.get("idle_loop"), ("hikari: idle loop %s" % loop) in log, "is not a state" in log, state)


def check_settings_face(port):
    vts_e2e.switch_model(port, MODEL)
    put = settings_call(port, {"face": "happy"})
    _, before = state_call(port, "GET", "")
    vts_e2e.switch_model(port, SPEAKER)
    _, after = state_call(port, "GET", "")
    settings_call(port, {"face": "neutral"})
    vts_e2e.switch_model(port, MODEL)
    ok = put[0] == 200 and json.loads(put[1])["face"] == "happy" and json.loads(before)["face"] == "happy" and json.loads(after)["face"] == "happy"
    return ok, "put=%s state_before_switch=%s after_switch=%s" % (put, before, after)


def png_bytes():
    raw = b"".join(b"\x00" + bytes([200, 100, 50]) * 8 for _ in range(8))
    chunks = [(b"IHDR", struct.pack(">IIBBBBB", 8, 8, 8, 2, 0, 0, 0)), (b"IDAT", zlib.compress(raw)), (b"IEND", b"")]
    return b"\x89PNG\r\n\x1a\n" + b"".join(struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data)) for kind, data in chunks)


def jpeg_bytes(png):
    with tempfile.TemporaryDirectory() as folder:
        with open(os.path.join(folder, "in.png"), "wb") as writer:
            writer.write(png)
        subprocess.run(["sips", "-s", "format", "jpeg", os.path.join(folder, "in.png"), "--out", os.path.join(folder, "out.jpg")], check=True, capture_output=True)
        with open(os.path.join(folder, "out.jpg"), "rb") as reader:
            return reader.read()


def put_background(port, body, content_type):
    conn = vts_e2e.http.client.HTTPConnection("127.0.0.1", port, timeout=vts_e2e.TIMEOUT)
    headers = {"Content-Type": content_type} if content_type else {}
    conn.request("PUT", "/background", body=body, headers=headers)
    resp = conn.getresponse()
    payload = resp.read().decode(errors="replace").strip()
    conn.close()
    return resp.status, json.loads(payload).get("type") if resp.status == 200 else payload


def check_background_sniff(port):
    png = png_bytes()
    cases = [
        ("png_no_type", png, None, (200, "image/png")),
        ("jpeg_no_type", jpeg_bytes(png), None, (200, "image/jpeg")),
        ("gif_no_type", GIF, None, (200, "image/gif")),
        ("png_as_gif", png, "image/gif", (200, "image/png")),
        ("text", b"hello there", "text/plain", (400, "hikari: background must be png, jpeg or gif")),
    ]
    got = {name: put_background(port, body, content_type) for name, body, content_type, _ in cases}
    vts_e2e.http_call(port, "DELETE", "/background")
    bad = {name: got[name] for name, _, _, want in cases if got[name] != want}
    return not bad, "got=%s bad=%s" % (got, bad)


def check_switch_gap(port):
    vts_e2e.switch_model(port, MODEL)
    with vts_e2e.concurrent.futures.ThreadPoolExecutor(1) as pool:
        stream = pool.submit(vts_e2e.read_frames, port, "GET", "/listen?fps=%d&codec=rgb565&size=160x120" % FPS, FPS * 6)
        time.sleep(1.5)
        vts_e2e.switch_model(port, SPEAKER)
        time.sleep(1.5)
        vts_e2e.switch_model(port, MODEL)
        status, frames, _, times = stream.result()
    gaps = [later - earlier for earlier, later in zip(times, times[1:])]
    worst = max(gaps, default=math.inf)
    return status == 200 and len(frames) == FPS * 6 and worst < GAP_LIMIT, "status=%d frames=%d/%d longest_gap_ms=%.1f need<%.1f (2 frames at %dfps) median_ms=%.1f" % (status, len(frames), FPS * 6, worst * 1000, GAP_LIMIT * 1000, FPS, sorted(gaps)[len(gaps) // 2] * 1000 if gaps else 0)


def log_actions(log_path, start):
    with open(log_path) as reader:
        return ACTION_LINE.findall(reader.read()[start:])


def check_get(port):
    status, body = state_call(port, "POST", "face=happy&pose=listening")
    get_status, got = state_call(port, "GET", "")
    want = {"face": "happy", "pose": "listening", "asleep": False}
    reset = state_call(port, "POST", "face=neutral&pose=none")
    _, listed = vts_e2e.http_call(port, "GET", "/state")
    states = json.loads(listed)
    shipped = ["neutral", "happy", "laughing", "sad", "crying", "angry", "doubtful", "excited", "grumpy", "confused", "thinking", "listening"]
    ok = status == 200 and get_status == 200 and json.loads(body) == want and want.items() <= json.loads(got).items() and json.loads(reset[1]) == START and all(name in states["states"] for name in shipped) and states["face"] == "neutral" and states["pose"] == "none"
    return ok, "post=%d %s get=%d %s reset=%s states=%s" % (status, body, get_status, got, reset[1], listed.decode())


def check_rejects(port):
    cases = [
        ("face=nosuch", "hikari: unknown state nosuch"),
        ("pose=dancing&face=happy", "hikari: unknown state dancing"),
        ("face=happy&pose=nosuch", "hikari: unknown state nosuch"),
    ]
    other = [
        ("PUT", "/view?zoom=1&save=", "hikari: save must be true or false"),
        ("PUT", "/view?zoom=1&save", "hikari: save must be true or false"),
        ("PUT", "/view?zoom=1&save=true&save=x", "hikari: save given more than once"),
    ]
    bad = []
    for query, line in cases:
        status, body = state_call(port, "POST", query)
        if status != 400 or body != line:
            bad.append((query, status, body))
    for method, path, line in other:
        status, body = vts_e2e.http_call(port, method, path)
        if status != 400 or body.decode().strip() != line:
            bad.append((path, status, body))
    for path in ["/idle?fps=24", "/render?fps=24", "/react?kind=pet"]:
        status, _ = vts_e2e.http_call(port, "GET" if path.startswith("/idle") else "POST", path)
        if status != 404:
            bad.append((path, status))
    _, got = state_call(port, "GET", "")
    return not bad and START.items() <= json.loads(got).items(), "cases=%d bad=%s state_after=%s" % (len(cases) + len(other) + 3, bad, got)


def check_idle_ignores(port):
    status, _, _, _ = vts_e2e.read_frames(port, "GET", "/listen?fps=%d&codec=rgb565&size=160x120&face=happy&pose=thinking" % FPS, 4)
    _, got = state_call(port, "GET", "")
    return status == 200 and START.items() <= json.loads(got).items(), "idle_status=%d state=%s" % (status, got)


def check_models(port):
    status, body = vts_e2e.http_call(port, "GET", "/models")
    listed = json.loads(body) if status == 200 else {}
    ok = listed.get("model") == MODEL and MODEL in listed.get("models", []) and SPEAKER in listed.get("models", [])
    return ok, "status=%d model=%s want=%s listed_%s=%s listed_%s=%s" % (status, listed.get("model"), MODEL, MODEL, MODEL in listed.get("models", []), SPEAKER, SPEAKER in listed.get("models", []))


def check_switch_resets(port):
    state_call(port, "POST", "face=happy&pose=listening")
    vts_e2e.switch_model(port, SPEAKER)
    _, got = state_call(port, "GET", "")
    model = current_settings(port).get("model")
    vts_e2e.switch_model(port, MODEL)
    return START.items() <= json.loads(got).items() and model == SPEAKER, "settings.model=%s state_after_switch=%s want=%s" % (model, got, START)


def check_settings_rejects(port):
    before = current_settings(port)
    cases = [{"model": "nosuchmodel%d" % os.getpid()}, {"size": [160, 120]}, {"port": PORT}, {"face": "nosuch"}, {"idle_loop": "x"}]
    bad = []
    for values in cases:
        status, body = settings_call(port, values)
        if status != 400:
            bad.append((values, status, body))
    after = current_settings(port)
    return not bad and after == before, "cases=%d bad=%s before=%s after=%s" % (len(cases), bad, before, after)


def render(port, body):
    status, frames, _, _ = vts_e2e.read_frames(port, "POST", "/talk?fps=%d&codec=rgb565&size=160x120&rate=1" % FPS, 400, body)
    return status, [vts_e2e.decode_rgb565(frame) for frame in frames]


def check_wav(port):
    pcm = vts_e2e.synth_pcm()
    silence = bytes(len(pcm))
    resampled = b"".join(pcm[int(i * vts_e2e.RATE / 44100) * 2:int(i * vts_e2e.RATE / 44100) * 2 + 2] * 2 for i in range(44100 * vts_e2e.PCM_SECONDS))
    runs = {
        "mono24000": render(port, vts_e2e.wav(pcm)),
        "silence": render(port, vts_e2e.wav(silence)),
        "stereo44100": render(port, vts_e2e.wav(resampled, 44100, 2)),
        "streamed": render(port, vts_e2e.wav(pcm, size=0xFFFFFFFF)),
    }
    motion = {name: vts_e2e.speech_motion(images) for name, (_, images) in runs.items()}
    frames = {name: len(images) for name, (_, images) in runs.items()}
    want = FPS * vts_e2e.PCM_SECONDS
    speech = [name for name in runs if name != "silence"]
    ok = all(runs[name][0] == 200 for name in runs) and all(abs(frames[name] - want) <= 1 for name in runs) and all(motion[name] > motion["silence"] + vts_e2e.MOTION_MARGIN for name in speech)
    return ok, "frames=%s want=%d speech_motion=%s" % (frames, want, {name: round(value, 2) for name, value in motion.items()})


def check_wav_rejects(port):
    pcm = vts_e2e.synth_pcm()[:4800]
    good = vts_e2e.wav(pcm)
    cases = [
        ("raw_pcm", pcm, "hikari: render body must be a RIFF/WAVE file"),
        ("24bit", good[:34] + b"\x18\x00" + good[36:], "hikari: render body must be 16-bit pcm wav, got 24-bit"),
        ("float", good[:20] + b"\x03\x00" + good[22:], "hikari: render body must be pcm wav (format 1), got format 3"),
        ("3ch", good[:22] + b"\x03\x00" + good[24:], "hikari: render body must have 1 or 2 channels, got 3"),
        ("rate4000", vts_e2e.wav(pcm, 4000), "hikari: render body rate must be 8000-96000, got 4000"),
        ("data_first", good[:12] + good[36:], "hikari: render body must have a fmt chunk before data"),
    ]
    bad = []
    for name, body, line in cases:
        status, payload = vts_e2e.http_call(port, "POST", "/talk?fps=%d&codec=rgb565&size=160x120" % FPS, body)
        if status != 400 or not payload.decode().startswith(line):
            bad.append((name, status, payload))
    return not bad, "cases=%d bad=%s" % (len(cases), bad)


def run(args, result):
    copies = []
    mouth = {"ParameterSettings": [{"Name": output, "Input": vts, "OutputLive2D": output} for vts, output in vts_e2e.ROUTES]}
    for name, source, vtube in ((MODEL, "Hiyori", {}), (SPEAKER, vts_e2e.SOURCE_MODEL, mouth)):
        target = os.path.join(vts_e2e.REPO, "models", name)
        if os.path.exists(target):
            raise RuntimeError("%s already exists" % target)
        copies.append(target)
        os.mkdir(target)
        vts_e2e.make_copy(target, name, source, vtube, still=True)
    with open(os.path.join(copies[0], "smirk.exp3.json"), "w") as writer:
        json.dump(SMIRK, writer)
    haru = os.path.join(vts_e2e.REPO, "models", HARU)
    copies.append(haru)
    os.mkdir(haru)
    vts_e2e.make_copy(haru, HARU, "Haru", {})
    over = os.path.join(vts_e2e.REPO, "models", OVERRIDE)
    copies.append(over)
    os.mkdir(over)
    vts_e2e.make_copy(over, OVERRIDE, "Hiyori", {}, still=True)
    shutil.copy(os.path.join(vts_e2e.REPO, "actions", "look_left.motion3.json"), os.path.join(over, "own_dance.motion3.json"))
    with open(os.path.join(over, OVERRIDE + ".model3.json")) as reader:
        model = json.load(reader)
    with open(os.path.join(over, "blush.motion3.json"), "w") as writer:
        json.dump(BLUSH, writer, indent="\t")
    model["FileReferences"]["Motions"] = {"dance": [{"File": "own_dance.motion3.json"}], "blush": [{"File": "blush.motion3.json"}]}
    with open(os.path.join(over, OVERRIDE + ".model3.json"), "w") as writer:
        json.dump(model, writer)
    with open(os.path.join(vts_e2e.REPO, "default.model3.json")) as reader:
        trimmed = json.load(reader)
    trimmed["FileReferences"]["Expressions"] = [dict(row, File=os.path.join(vts_e2e.REPO, row["File"])) for row in trimmed["FileReferences"]["Expressions"]]
    trimmed["FileReferences"]["Motions"] = {name: [{"File": os.path.join(vts_e2e.REPO, row["File"])} for row in rows] for name, rows in trimmed["FileReferences"]["Motions"].items() if name != "dance"}
    handle = None
    log_path = args.out + ".log"
    try:
        handle = vts_e2e.start_server(args.binary, args.port, log_path, model=MODEL, settings={"idle_actions": BASIC + ["sleep"]})
        settings_call(args.port, {"action_every": QUIET_EVERY})
        if args.only == "zd_action_params_rest":
            settings_call(args.port, {"idle_actions": []})
            result.add("zd_action_params_rest", check_action_params_rest(args.port, OVERRIDE))
            return
        result.add("y_default_actions_play", check_default_actions(args.port))
        if args.only:
            return
        settings_call(args.port, {"idle_actions": BASIC + ["sleep"]})
        result.add("i_models_listed", check_models(args.port))
        result.add("f_idle_ignores_face", check_idle_ignores(args.port))
        actions = current_settings(args.port)["idle_actions"]
        settings_call(args.port, {"idle_actions": []})
        for label, model in (("hiyori", MODEL), ("mao", SPEAKER)):
            result.add("a_pose_thinking_crop_%s" % label, check_change(args.port, model, ["pose=thinking", "pose=none"]))
            result.add("b_face_happy_crop_%s" % label, check_change(args.port, model, ["face=happy", "face=neutral"]))
        result.add("n_action_yawn_now", check_action_now(args.port, MODEL, "yawn"))
        settings_call(args.port, {"idle_actions": actions})
        vts_e2e.switch_model(args.port, MODEL)
        result.add("l_action_contract", check_action_contract(args.port))
        result.add("m_get_actions", check_get_actions(args.port))
        result.add("z_model_overrides_default", check_model_overrides_default(args.port, OVERRIDE))
        result.add("zb_action_every_range", check_every_range(args.port, log_path))
        settings_call(args.port, {"idle_actions": actions})
        vts_e2e.switch_model(args.port, MODEL)
        result.add("o_idle_dance", check_idle_dance(args.port, log_path))
        result.add("p_idle_sleep_rejected", check_idle_sleep_rejected(args.port, log_path))
        result.add("q_sleep_action", check_sleep_action(args.port, log_path))
        settings_call(args.port, {"idle_actions": [], "action_every": QUIET_EVERY})
        result.add("t_custom_smirk", check_smirk(args.port))
        result.add("v_tapbody_haru", check_tapbody(args.port, HARU))
        result.add("zd_action_params_rest", check_action_params_rest(args.port, OVERRIDE))
        result.add("x_settings_face", check_settings_face(args.port))
        vts_e2e.switch_model(args.port, MODEL)
        result.add("r_background_sniff", check_background_sniff(args.port))
        result.add("s_switch_gap", check_switch_gap(args.port))
        result.add("d_get_state", check_get(args.port))
        result.add("e_rejects", check_rejects(args.port))
        result.add("j_switch_resets_state", check_switch_resets(args.port))
        result.add("k_settings_rejects", check_settings_rejects(args.port))
        vts_e2e.switch_model(args.port, SPEAKER)
        result.add("g_wav_render", check_wav(args.port))
        result.add("h_wav_rejects", check_wav_rejects(args.port))
        vts_e2e.stop_server(handle)
        handle = None
        result.add("w_idle_loop", check_idle_loop(args, HARU, "haru_g_m15"))
        result.add("zc_trimmed_defaults", check_trimmed_defaults(args, trimmed))
    finally:
        try:
            if handle:
                vts_e2e.stop_server(handle)
        finally:
            for copy in copies:
                shutil.rmtree(copy, ignore_errors=True)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True)
    parser.add_argument("--out", required=True)
    parser.add_argument("--port", type=int, default=PORT)
    parser.add_argument("--only", choices=["y_default_actions_play", "zd_action_params_rest"])
    args = parser.parse_args()
    if args.port == vts_e2e.LIVE_PORT:
        parser.error("port %d is the live service" % vts_e2e.LIVE_PORT)
    result = vts_e2e.Result(args.out)
    try:
        run(args, result)
    except BaseException as error:
        result.payload["error"] = "%s: %s" % (type(error).__name__, error)
        result.flush()
        raise
    return vts_e2e.finish(result)


if __name__ == "__main__":
    sys.exit(main())
