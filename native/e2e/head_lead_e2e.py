#!/usr/bin/env python3
import argparse
import json
import math
import os
import shutil
import statistics
import sys
import threading

import vts_e2e

PORT = 9326
MODEL = "leadhiyori%d" % os.getpid()
ACTION = "late_left"
FPS = vts_e2e.FPS
FRAME_MS = 1000.0 / FPS
LEAD_MS = 400
ACTION_AT = 1.5
STREAM_SECONDS = 5.5
BASE_FRAMES = 24
TRIALS = 3
HOLD_FRAMES = (int((ACTION_AT + 1.6) * FPS), int((ACTION_AT + 2.3) * FPS))
YAW_TOLERANCE = 2
LATE_LEFT = {
    "Version": 3,
    "Meta": {"Duration": 3.0, "Fps": 30.0, "FadeInTime": 0.0, "FadeOutTime": 0.0, "Loop": False, "AreBeziersRestricted": False, "CurveCount": 1, "TotalSegmentCount": 5, "TotalPointCount": 6, "UserDataCount": 0, "TotalUserDataSize": 0},
    "Curves": [{"Target": "Parameter", "Id": "ParamAngleX", "Segments": [0.0, 0.0, 0, 1.0, 0.0, 0, 1.2, 28.0, 0, 2.5, 28.0, 0, 2.7, 0.0, 0, 3.0, 0.0]}],
}


def crossing(values):
    base = statistics.median(values[:BASE_FRAMES])
    peak = max(values[BASE_FRAMES:], key=lambda value: abs(value - base))
    half = (base + peak) / 2
    for index in range(BASE_FRAMES, len(values)):
        if abs(values[index] - base) >= abs(half - base):
            before = values[index - 1]
            return index - 1 + (half - before) / (values[index] - before)
    return math.nan


def trial(port):
    timer = threading.Timer(ACTION_AT, vts_e2e.http_call, (port, "POST", "/action?action=" + ACTION))
    timer.start()
    status, frames, heads, _ = vts_e2e.read_frames(port, "GET", "/listen?fps=%d&codec=rgb565&size=%dx%d" % (FPS, vts_e2e.WIDTH, vts_e2e.HEIGHT), int(STREAM_SECONDS * FPS))
    timer.join()
    yaws = [head[0] for head in heads]
    hold = yaws[HOLD_FRAMES[0]:HOLD_FRAMES[1]]
    return {"status": status, "frames": len(frames), "onset_ms": round(crossing(yaws) * FRAME_MS - ACTION_AT * 1000), "base_yaw": statistics.median(yaws[:BASE_FRAMES]), "hold_yaw": statistics.median(hold) if hold else math.nan}


def set_lead(port, value):
    status, body = vts_e2e.http_call(port, "POST", "/state?head_lead_ms=%s" % value)
    return status, body.decode(errors="replace").strip()


def check_api(port):
    high = set_lead(port, 1501)
    word = set_lead(port, "abc")
    status, body = set_lead(port, LEAD_MS)
    posted = json.loads(body) if status == 200 else {}
    got_status, got = vts_e2e.http_call(port, "GET", "/state")
    listed = json.loads(got) if got_status == 200 else {}
    zero_status, zero_body = set_lead(port, 0)
    zero = json.loads(zero_body) if zero_status == 200 else {}
    ok = high[0] == 400 and "head_lead_ms" in high[1] and word[0] == 400 and "head_lead_ms" in word[1] and posted.get("head_lead_ms") == LEAD_MS and listed.get("head_lead_ms") == LEAD_MS and zero.get("head_lead_ms") == 0
    return ok, "1501=%d %s abc=%d post=%s get=%s zero=%s" % (high[0], high[1], word[0], body, got.decode(errors="replace"), zero_body)


def summary(rows):
    return {key: statistics.median(row[key] for row in rows) for key in ("onset_ms", "base_yaw", "hold_yaw")}


def make_model():
    target = os.path.join(vts_e2e.REPO, "models", MODEL)
    os.mkdir(target)
    vts_e2e.make_copy(target, MODEL, "Hiyori", {}, still=True)
    with open(os.path.join(target, ACTION + ".motion3.json"), "w") as writer:
        json.dump(LATE_LEFT, writer, indent="\t")
    path = os.path.join(target, MODEL + ".model3.json")
    with open(path) as reader:
        model = json.load(reader)
    model["FileReferences"]["Motions"] = {ACTION: [{"File": ACTION + ".motion3.json"}]}
    with open(path, "w") as writer:
        json.dump(model, writer)
    return target


def serve(binary, port, log_path, leads):
    handle = vts_e2e.start_server(binary, port, log_path, model=MODEL, settings={"idle_actions": []})
    try:
        api = check_api(port) if None not in leads else None
        rows = {lead: [] for lead in leads}
        for _ in range(TRIALS):
            for lead in leads:
                if lead is not None:
                    set_lead(port, lead)
                rows[lead].append(trial(port))
        return api, rows
    finally:
        vts_e2e.stop_server(handle)


def run(args, result):
    target = make_model()
    try:
        api, rows = serve(args.binary, args.port, args.out + ".log", [0, LEAD_MS])
        _, main_rows = serve(args.main_binary, args.port, args.out + ".main.log", [None])
    finally:
        shutil.rmtree(target, ignore_errors=True)
    result.add("a_head_lead_api", api)
    zero, lead, main = summary(rows[0]), summary(rows[LEAD_MS]), summary(main_rows[None])
    statuses = [row["status"] for row in rows[0] + rows[LEAD_MS] + main_rows[None]]
    shift = zero["onset_ms"] - lead["onset_ms"]
    detail = "yaw half-turn onset ms after POST (curve turns at 1000-1200) lead0=%s lead%d=%s main=%s shift=%s need %d+-%.0f statuses=%s" % (zero["onset_ms"], LEAD_MS, lead["onset_ms"], main["onset_ms"], shift, LEAD_MS, FRAME_MS, sorted(set(statuses)))
    result.add("b_yaw_leads_by_head_lead_ms", (set(statuses) == {200} and abs(shift - LEAD_MS) <= FRAME_MS, detail))
    same = abs(zero["onset_ms"] - main["onset_ms"]) <= FRAME_MS and abs(zero["base_yaw"] - main["base_yaw"]) <= YAW_TOLERANCE and abs(zero["hold_yaw"] - main["hold_yaw"]) <= YAW_TOLERANCE and abs(lead["hold_yaw"] - zero["hold_yaw"]) <= YAW_TOLERANCE
    result.add("c_lead0_matches_main", (same, "lead0 %s main %s lead%d %s need onset within %.0f ms and base/hold yaw within %d" % (zero, main, LEAD_MS, lead, FRAME_MS, YAW_TOLERANCE)))
    result.payload["trials"] = {"lead0": rows[0], "lead%d" % LEAD_MS: rows[LEAD_MS], "main": main_rows[None]}
    result.flush()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True)
    parser.add_argument("--main-binary", required=True)
    parser.add_argument("--out", required=True)
    parser.add_argument("--port", type=int, default=PORT)
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
