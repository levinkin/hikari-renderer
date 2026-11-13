#!/usr/bin/env python3
import argparse
import json
import os
import shutil
import statistics
import sys
import time

import vts_e2e

PORT = 9327
SOURCE = "Mao"
FPS = vts_e2e.FPS
SETTLE_FRAMES = FPS
WIND_FRAMES = FPS * 8
HEAD_FRAMES = int(6.5345 * FPS)
HEAD_SETTLE = FPS // 2
HEAD_ORDER = (30, -30, -30, 30)
HAIR_ROWS = (40, 120)
HAIR_COLS = (40, 120)
HEAD_ROWS = (30, 90)
HEAD_COLS = (40, 120)
CONFIGS = {"calm": (50, 0), "windy": (50, 100), "max": (100, 100)}
WIND_GAIN = 2.5
PUSH_GAIN = 1.0
STRENGTH_GAIN = 0.5
SAME_TOLERANCE = 0.1
HEAD_MARGIN = 5.0
HOLD = {"ParamMouthOpenY": 0.6, "ParamEyeLOpen": 1, "ParamEyeROpen": 1, "ParamEyeLSmile": 0, "ParamEyeRSmile": 0, "ParamBrowLY": 0, "ParamBrowRY": 0, "ParamCheek": 0, "ParamAngleX": 0, "ParamAngleY": 0, "ParamAngleZ": 0, "ParamBodyAngleX": 0, "ParamBreath": 0}
FACE_ROWS = (80, 110)
FACE_COLS = (64, 98)
HOLD_FRAMES = FPS * 2
HOLD_LIMIT = 1.0
HOLD_MARGIN = 2.0
WAKE_FRAMES = FPS // 2
WAKE_RATIO = 4.0
GRAVITY_MARGIN = 2.0
direction_margin = 1.0
FLOOD_CALLS = 2000
FLOOD_RSS_KB = 2048
sweep_directions = range(150, 211)
sweep_frames = FPS // 4
sweep_ratio = 3.0
turn_directions = range(0, 361, 2)
turn_frames = HEAD_FRAMES * 2
reset_ratio = 2.0
lean_ratio = 5.0


def crop(image, rows, cols):
    stride = vts_e2e.WIDTH * 3
    pixels = []
    for y in range(*rows):
        pixels.extend(image[y * stride + cols[0] * 3:y * stride + cols[1] * 3])
    return pixels


def crop_drift(images):
    crops = [crop(image, HAIR_ROWS, HAIR_COLS) for image in images]
    pairs = list(zip(crops, crops[FPS // 2:]))
    if not pairs:
        return 0.0
    return sum(sum(abs(a - b) for a, b in zip(first, second)) for first, second in pairs) / (len(pairs) * len(crops[0]))


def make_models(targets):
    settings = {key: {"PhysicsSettings": {"Use": True, "PhysicsStrength": strength, "WindStrength": wind}} for key, (strength, wind) in CONFIGS.items()}
    settings["still"] = {"PhysicsSettings": {"Use": True, "PhysicsStrength": 0, "WindStrength": 0}, "ParameterSettings": [{"Name": output, "Input": vts, "OutputLive2D": output} for vts, output in vts_e2e.ROUTES]}
    for key, vtube in settings.items():
        target = os.path.join(vts_e2e.REPO, "models", "windparams_" + key)
        os.mkdir(target)
        targets.append(target)
        vts_e2e.make_copy(target, "windparams_" + key, SOURCE, vtube, still=True)
    target = os.path.join(vts_e2e.REPO, "models", "windparams_sweep")
    os.mkdir(target)
    targets.append(target)
    vts_e2e.make_copy(target, "windparams_sweep", vts_e2e.WIND_MODEL, {"PhysicsSettings": {"Use": True, "PhysicsStrength": 50, "WindStrength": 0}}, still=True)


def frames_for(port, key, count):
    vts_e2e.switch_model(port, "windparams_" + key)
    vts_e2e.idle_frames(port, SETTLE_FRAMES)
    return vts_e2e.idle_frames(port, count)[1]


def post_params(port, body):
    status, payload = vts_e2e.http_call(port, "POST", "/params", json.dumps(body).encode())
    text = payload.decode(errors="replace").strip()
    return status, json.loads(text) if status == 200 else text


def post_physics(port, body):
    status, payload = vts_e2e.http_call(port, "POST", "/physics", body if isinstance(body, bytes) else json.dumps(body).encode())
    text = payload.decode(errors="replace").strip()
    return status, json.loads(text) if status == 200 else text


def physics_frames(port, body, count):
    post_physics(port, body)
    vts_e2e.idle_frames(port, SETTLE_FRAMES)
    return vts_e2e.idle_frames(port, count)[1]


def check_physics_errors(port):
    cases = [(b"[1]", "needs a JSON object"), (b"nope", "needs a JSON object"), (b'{"wind": "high"}', "wind must be an object"), (b'{"wind": 100}', "wind must be an object"), (b'{"strength": null}', "strength must be a number"), (b'{"gravity": true}', "gravity must be an object"), (b'{"wind": {"strength": 1e39}}', "wind.strength must be a number"), (b'{"wind": {"direction": "up"}}', "wind.direction must be a number"), (b'{"wind": {"speed": 1}}', "unknown physics key wind.speed"), (b'{"gravity": {"strength": 1}}', "unknown physics key gravity.strength"), (b'{"tilt": 1}', "unknown physics key tilt")]
    replies = [post_physics(port, body) for body, _ in cases]
    ok = all(reply[0] == 400 and want in reply[1] for reply, (_, want) in zip(replies, cases))
    return ok, "400 cases=%d statuses=%s" % (len(cases), [reply[0] for reply in replies])


def check_physics_partial(port):
    first = post_physics(port, {"wind": {"strength": 100}})
    second = post_physics(port, {"wind": {"direction": 45}, "gravity": {"direction": 90}})
    ok = first[0] == 200 and second == (200, {"strength": 50, "wind": {"strength": 100, "direction": 45}, "gravity": {"direction": 90}})
    return ok, "after wind.strength=100 then wind.direction=45 gravity.direction=90 reply=%s" % (second,)


def check_physics_visible(port):
    vts_e2e.switch_model(port, "windparams_calm")
    most = crop_drift(physics_frames(port, {"strength": 100, "wind": {"strength": 100}, "gravity": {"direction": 0}}, WIND_FRAMES))
    windy = crop_drift(physics_frames(port, {"strength": 50}, WIND_FRAMES))
    calm = crop_drift(physics_frames(port, {"wind": {"strength": 0}}, WIND_FRAMES))
    down = [crop(image, HAIR_ROWS, HAIR_COLS) for image in physics_frames(port, {"gravity": {"direction": 0}}, FPS * 2)]
    side = [crop(image, HAIR_ROWS, HAIR_COLS) for image in physics_frames(port, {"gravity": {"direction": 90}}, FPS * 2)]
    post_physics(port, {"gravity": {"direction": 0}})
    tilt = vts_e2e.image_diff(vts_e2e.mean_image(down), vts_e2e.mean_image(side))
    control = vts_e2e.image_diff(vts_e2e.mean_image(down[:FPS]), vts_e2e.mean_image(down[FPS:]))
    runs = []
    for strength in (100, 100, 300):
        vts_e2e.switch_model(port, "windparams_calm")
        vts_e2e.switch_model(port, "windparams_windy")
        runs.append(vts_e2e.mean_image([crop(image, HAIR_ROWS, HAIR_COLS) for image in physics_frames(port, {"strength": strength}, WIND_FRAMES)]))
    repeat = vts_e2e.image_diff(runs[0], runs[1])
    past = vts_e2e.image_diff(runs[0], runs[2])
    vts_e2e.switch_model(port, "windparams_calm")
    wind_ok = most > calm + WIND_GAIN and windy > calm and past > repeat + PUSH_GAIN
    gravity_ok = tilt > control + GRAVITY_MARGIN
    return (wind_ok, "hair crop drift post wind=100=%.3f wind=0=%.3f strength=100+wind=100=%.3f need wind100 > wind0 and max > wind0 + %.2f; hair crop strength 300 vs 100=%.3f repeat 100 vs 100=%.3f need > repeat + %.1f" % (windy, calm, most, WIND_GAIN, past, repeat, PUSH_GAIN)), (gravity_ok, "hair crop mean diff gravity 90 vs 0=%.3f control=%.3f need > control + %.1f" % (tilt, control, GRAVITY_MARGIN))


def check_physics_switch(port):
    vts_e2e.switch_model(port, "windparams_calm")
    set_reply = post_physics(port, {"strength": 80, "wind": {"strength": 30, "direction": 90}, "gravity": {"direction": 45}})
    vts_e2e.switch_model(port, "windparams_windy")
    vts_e2e.idle_frames(port, 2)
    reset = post_physics(port, {})
    ok = set_reply[0] == 200 and reset == (200, {"strength": 50, "wind": {"strength": 100, "direction": 90}, "gravity": {"direction": 0}})
    return ok, "set=%s after switch to windparams_windy reply=%s" % (set_reply, reset)


def direction_crop(port, body):
    vts_e2e.switch_model(port, "windparams_calm")
    vts_e2e.switch_model(port, "windparams_windy")
    return vts_e2e.mean_image([crop(image, HAIR_ROWS, HAIR_COLS) for image in physics_frames(port, body, WIND_FRAMES)])


def check_wind_direction(port):
    runs = [direction_crop(port, {"wind": {"strength": 100, "direction": direction}}) for direction in (90, 90, 270)]
    repeat = vts_e2e.image_diff(runs[0], runs[1])
    turned = vts_e2e.image_diff(runs[0], runs[2])
    vts_e2e.switch_model(port, "windparams_calm")
    return turned > repeat + direction_margin, "hair crop mean diff wind direction 270 vs 90=%.3f repeat 90 vs 90=%.3f need > repeat + %.1f" % (turned, repeat, direction_margin)


def get_settings(port):
    status, payload = vts_e2e.http_call(port, "GET", "/settings")
    return json.loads(payload) if status == 200 else {}


def put_settings(port, body):
    status, payload = vts_e2e.http_call(port, "PUT", "/settings", json.dumps(body).encode())
    text = payload.decode(errors="replace").strip()
    return status, json.loads(text) if status == 200 else text


def check_settings_physics(binary, port, out, result):
    fallback = {"strength": 70, "wind": {"strength": 30, "direction": 270}}
    handle = vts_e2e.start_server(binary, port, out + ".settings.log", model="windparams_windy", settings={"idle_actions": [], "physics": fallback})
    try:
        own = post_physics(port, {})
        listed = get_settings(port).get("physics")
        result.add("o_settings_own_wind_wins", (own == (200, {"strength": 50, "wind": {"strength": 100, "direction": 270}, "gravity": {"direction": 0}}) and listed == fallback, "windparams_windy has vtube strength 50 wind 100, settings physics=%s; POST {} reply=%s GET /settings physics=%s" % (fallback, own, listed)))
        runs = [direction_crop(port, body) for body in ({}, {"wind": {"direction": 270}}, {"wind": {"direction": 90}})]
        same = vts_e2e.image_diff(runs[0], runs[1])
        other = vts_e2e.image_diff(runs[0], runs[2])
        result.add("p_settings_wind_direction", (other > same + direction_margin, "hair crop mean diff settings direction 270 vs POST 270=%.3f vs POST 90=%.3f need > same + %.1f" % (same, other, direction_margin)))
        post_physics(port, {"strength": 80})
        wanted = {"wind": {"direction": 90}, "gravity": {"direction": 270}}
        bad = put_settings(port, {"physics": {"wind": {"direction": "up"}}})
        reply = put_settings(port, {"physics": wanted})
        applied = post_physics(port, {})
        with open(os.path.join(handle["run_dir"], "settings.json")) as reader:
            saved = json.load(reader)
    finally:
        vts_e2e.stop_server(handle)
    restart = {key: value for key, value in saved.items() if key not in ("port", "model")}
    handle = vts_e2e.start_server(binary, port, out + ".restart.log", model="windparams_windy", settings=restart)
    try:
        listed = get_settings(port).get("physics")
        again = post_physics(port, {})
    finally:
        vts_e2e.stop_server(handle)
    expect = (200, {"strength": 50, "wind": {"strength": 100, "direction": 90}, "gravity": {"direction": 270}})
    ok = bad[0] == 400 and "wind.direction must be a number" in bad[1] and reply[0] == 200 and reply[1].get("physics") == wanted and applied == expect and saved.get("physics") == wanted and listed == wanted and again == expect
    result.add("q_put_settings_physics_persists", (ok, "bad PUT=%s PUT reply physics=%s POST {} after PUT over strength 80=%s settings.json physics=%s after restart GET physics=%s POST {}=%s" % (bad, reply[1].get("physics") if reply[0] == 200 else reply, applied, saved.get("physics"), listed, again)))
    log_path = out + ".badsettings.log"
    handle = vts_e2e.start_server(binary, port, log_path, model="windparams_windy", settings={"idle_actions": [], "physics": {"wind": {"direction": "up"}}})
    try:
        listed = get_settings(port).get("physics")
        fell = post_physics(port, {})
    finally:
        vts_e2e.stop_server(handle)
    with open(log_path) as reader:
        lines = [line.strip() for line in reader if "settings.json physics" in line]
    ok = len(lines) == 1 and listed == {} and fell == (200, {"strength": 50, "wind": {"strength": 100, "direction": 90}, "gravity": {"direction": 0}})
    result.add("r_bad_settings_physics_starts", (ok, "log lines=%s GET physics=%s POST {}=%s" % (lines, listed, fell)))


def check_physics_flood(port, pid):
    vts_e2e.idle_frames(port, 2)
    for _ in range(50):
        post_physics(port, {"wind": {"strength": 0}})
    before = vts_e2e.process_usage(pid)[0]
    statuses = []
    started = time.monotonic()
    for index in range(FLOOD_CALLS):
        statuses.append(post_physics(port, {"wind": {"strength": index % 100, "direction": index % 360}, "gravity": {"direction": index % 360}})[0])
    elapsed = time.monotonic() - started
    vts_e2e.idle_frames(port, 2)
    after = vts_e2e.process_usage(pid)[0]
    ok = statuses.count(200) == FLOOD_CALLS and after - before <= FLOOD_RSS_KB
    return ok, "calls=%d ok=%d rps=%.0f rss_kb before=%d after=%d delta=%d need <=%d" % (FLOOD_CALLS, statuses.count(200), FLOOD_CALLS / elapsed, before, after, after - before, FLOOD_RSS_KB)


def check_gravity_sweep(port):
    vts_e2e.switch_model(port, "windparams_sweep")
    post_physics(port, {"gravity": {"direction": sweep_directions[0]}})
    vts_e2e.idle_frames(port, SETTLE_FRAMES)
    means = []
    for direction in sweep_directions:
        post_physics(port, {"gravity": {"direction": direction}})
        means.append(vts_e2e.mean_image([crop(image, HAIR_ROWS, HAIR_COLS) for image in vts_e2e.idle_frames(port, sweep_frames)[1]]))
    post_physics(port, {"gravity": {"direction": 0}})
    vts_e2e.switch_model(port, "windparams_calm")
    jumps = [vts_e2e.image_diff(first, second) for first, second in zip(means, means[1:])]
    largest = max(jumps)
    median = statistics.median(jumps)
    worst = sweep_directions[jumps.index(largest)]
    return largest < median * sweep_ratio, "hair crop mean step jump gravity %d..%d max=%.3f at %d->%d median=%.3f need max < median * %.1f" % (sweep_directions[0], sweep_directions[-1], largest, worst, worst + 1, median, sweep_ratio)


def settled_hair(port):
    vts_e2e.idle_frames(port, SETTLE_FRAMES)
    return vts_e2e.mean_image([crop(image, HAIR_ROWS, HAIR_COLS) for image in vts_e2e.idle_frames(port, turn_frames)[1]])


def check_gravity_jump_reset(port):
    vts_e2e.switch_model(port, "windparams_calm")
    vts_e2e.switch_model(port, "windparams_sweep")
    post_params(port, HOLD)
    reference = settled_hair(port)
    repeat = settled_hair(port)
    for direction in turn_directions:
        post_physics(port, {"gravity": {"direction": direction}})
        vts_e2e.idle_frames(port, sweep_frames)
    turned = settled_hair(port)
    post_physics(port, {"gravity": {"direction": 0}})
    after = settled_hair(port)
    post_params(port, {key: None for key in HOLD})
    vts_e2e.switch_model(port, "windparams_calm")
    noise = vts_e2e.image_diff(reference, repeat)
    reset = vts_e2e.image_diff(after, reference)
    lean = vts_e2e.image_diff(turned, reference)
    ok = reset < noise * reset_ratio and lean > noise * lean_ratio
    return ok, "hair crop mean vs fresh gravity 0: after sweep 0..360 then post 0=%.3f at 360=%.3f repeat noise=%.3f need after < noise * %.1f and at 360 > noise * %.1f" % (reset, lean, noise, reset_ratio, lean_ratio)


def check_physics(port, pid, result):
    vts_e2e.switch_model(port, "windparams_calm")
    vts_e2e.idle_frames(port, 2)
    result.add("h_physics_400s", check_physics_errors(port))
    result.add("i_physics_partial_keeps", check_physics_partial(port))
    wind, gravity = check_physics_visible(port)
    result.add("j_physics_wind_visible", wind)
    result.add("k_physics_gravity_visible", gravity)
    result.add("l_physics_switch_resets", check_physics_switch(port))
    result.add("m_physics_flood_cheap", check_physics_flood(port, pid))
    result.add("n_physics_wind_direction", check_wind_direction(port))
    result.add("s_gravity_sweep_continuous", check_gravity_sweep(port))
    result.add("t_gravity_jump_resets_turns", check_gravity_jump_reset(port))


def head_frames(port, value):
    post_params(port, {"ParamAngleX": value})
    vts_e2e.idle_frames(port, HEAD_SETTLE)
    return vts_e2e.idle_frames(port, HEAD_FRAMES)[1]


def check_api(port):
    unknown = post_params(port, {"NoSuchParam": 1, "ParamAngleX": 1})
    word = post_params(port, {"ParamAngleX": "left"})
    clamped = post_params(port, {"ParamAngleX": 999, "ParamAngleY": 5})
    released = post_params(port, {"ParamAngleY": None, "ParamAngleX": None})
    ok = unknown[0] == 400 and "NoSuchParam" in unknown[1] and "ParamAngleX" not in unknown[1] and word[0] == 400 and clamped[0] == 200 and 1 < clamped[1].get("ParamAngleX", 999) < 999 and clamped[1].get("ParamAngleY") == 5 and released == (200, {})
    return ok, "unknown=%s word=%s clamped=%s released=%s" % (unknown, word, clamped, released)


def face_frames(port, face, hold):
    post_params(port, hold)
    vts_e2e.http_call(port, "POST", "/state?face=" + face)
    vts_e2e.idle_frames(port, HEAD_SETTLE)
    if face == "neutral":
        return vts_e2e.idle_frames(port, HOLD_FRAMES)[1]
    return vts_e2e.render_images(port, vts_e2e.synth_pcm())[1]


def check_hold(port):
    vts_e2e.switch_model(port, "windparams_still")
    released = {key: None for key in HOLD}
    spreads = []
    for hold in (HOLD, released):
        still = vts_e2e.mean_image([crop(image, FACE_ROWS, FACE_COLS) for image in face_frames(port, "neutral", hold)])
        talking = [crop(image, FACE_ROWS, FACE_COLS) for image in face_frames(port, "laughing", hold)]
        if not talking:
            vts_e2e.http_call(port, "POST", "/state?face=neutral")
            return False, "no talking frames rendered"
        spreads.append(sum(vts_e2e.image_diff(image, still) for image in talking) / len(talking))
    vts_e2e.http_call(port, "POST", "/state?face=neutral")
    ok = spreads[0] < HOLD_LIMIT and spreads[1] > spreads[0] + HOLD_MARGIN
    return ok, "face crop laughing+talking vs neutral idle diff held=%.3f need <%.2f released=%.3f need > held + %.1f" % (spreads[0], HOLD_LIMIT, spreads[1], HOLD_MARGIN)


def check_wake_hold(port):
    post_params(port, dict(HOLD, ParamAngleX=-30))
    vts_e2e.http_call(port, "POST", "/action?action=sleep")
    asleep = vts_e2e.idle_frames(port, FPS)[1]
    vts_e2e.http_call(port, "POST", "/state?face=neutral")
    post_params(port, {"ParamAngleX": 30})
    waking = [crop(image, HEAD_ROWS, HEAD_COLS) for image in vts_e2e.idle_frames(port, WAKE_FRAMES)[1]]
    held = vts_e2e.mean_image([crop(image, HEAD_ROWS, HEAD_COLS) for image in vts_e2e.idle_frames(port, HOLD_FRAMES)[1][-FPS:]])
    post_params(port, {key: None for key in HOLD})
    turn = vts_e2e.image_diff(vts_e2e.mean_image([crop(image, HEAD_ROWS, HEAD_COLS) for image in asleep[-FPS // 2:]]), held)
    worst = max(vts_e2e.image_diff(image, held) for image in waking)
    ok = len(waking) == WAKE_FRAMES and turn > HEAD_MARGIN and worst < turn / WAKE_RATIO
    return ok, "head crop ParamAngleX -30 asleep vs 30 held=%.3f worst wake-carry frame vs held=%.3f need < turn / %.1f and turn > %.1f" % (turn, worst, WAKE_RATIO, HEAD_MARGIN)


def serve(binary, port, log_path, keys, params, result):
    handle = vts_e2e.start_server(binary, port, log_path, model="windparams_calm", settings={"idle_actions": []})
    try:
        frames = {key: frames_for(port, key, WIND_FRAMES) for key in keys}
        if not params:
            return frames, None, None, None, None
        check_physics(port, handle["proc"].pid, result)
        api = check_api(port)
        heads = [head_frames(port, value) for value in HEAD_ORDER]
        post_params(port, {"ParamAngleX": None})
        return frames, api, heads, check_hold(port), check_wake_hold(port)
    finally:
        vts_e2e.stop_server(handle)


def run(args, result):
    targets = []
    try:
        make_models(targets)
        frames, api, heads, hold, wake = serve(args.binary, args.port, args.out + ".log", list(CONFIGS), True, result)
        check_settings_physics(args.binary, args.port, args.out, result)
        main_frames, _, _, _, _ = serve(args.main_binary, args.port, args.out + ".main.log", ["calm"], False, result)
    finally:
        for target in targets:
            shutil.rmtree(target, ignore_errors=True)
    drift = {key: crop_drift(images) for key, images in frames.items()}
    main_calm = crop_drift(main_frames["calm"])
    detail = "hair crop drift calm=%.3f windy=%.3f max=%.3f main_calm=%.3f" % (drift["calm"], drift["windy"], drift["max"], main_calm)
    result.add("a_wind_blows_hair", (drift["max"] > drift["calm"] + WIND_GAIN and drift["windy"] > drift["calm"], "%s need max > calm + %.2f" % (detail, WIND_GAIN)))
    result.add("b_strength_max_moves_more", (drift["max"] > drift["windy"] + STRENGTH_GAIN, "%s need max > windy + %.2f" % (detail, STRENGTH_GAIN)))
    same = vts_e2e.image_diff(vts_e2e.mean_image(frames["calm"]), vts_e2e.mean_image(main_frames["calm"]))
    result.add("c_wind0_matches_main", (same < SAME_TOLERANCE and abs(drift["calm"] - main_calm) < SAME_TOLERANCE, "whole-frame mean diff=%.4f %s need both under %.2f" % (same, detail, SAME_TOLERANCE)))
    result.add("d_params_api", api)
    crops = [[crop(image, HEAD_ROWS, HEAD_COLS) for image in images] for images in heads]
    right = vts_e2e.mean_image(crops[0] + crops[3])
    left = vts_e2e.mean_image(crops[1] + crops[2])
    turn = vts_e2e.image_diff(right, left)
    control = vts_e2e.image_diff(vts_e2e.mean_image(crops[0]), vts_e2e.mean_image(crops[3]))
    result.add("e_params_turn_head", (all(len(images) == HEAD_FRAMES for images in heads) and turn > control + HEAD_MARGIN, "head crop diff ParamAngleX 30 vs -30=%.3f 30 first vs last window=%.3f need turn > control + %.1f" % (turn, control, HEAD_MARGIN)))
    result.add("f_params_hold_over_face_and_mouth", hold)
    result.add("g_params_hold_during_wake_carry", wake)
    result.payload["drift"] = dict(drift, main_calm=main_calm)
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
