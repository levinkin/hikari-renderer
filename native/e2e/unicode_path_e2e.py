#!/usr/bin/env python3
import argparse
import json
import os
import shutil
import struct
import sys

import vts_e2e

PORT = 9420
SOURCE = "Mao"
ASCII = "uniascii%d" % os.getpid()
UNICODE = "测试%d" % os.getpid()
bad = "unibad%d" % os.getpid()
bad_source = "Hiyori"
bad_offset = 64
bad_word = 0x7ffffff0
RENAMES = [(SOURCE, UNICODE), ("texture", "纹理"), ("expressions", "表情"), ("motions", "动作"), ("mtn_", "动_"), ("exp_", "情_"), ("special_", "特_"), ("sample_", "样_")]
FRAMES = 48
SETTLE_FRAMES = 12
SAME_LIMIT = 1.0
BLANK_FLOOR = 8.0


def renamed(text):
    for old, new in RENAMES:
        text = text.replace(old, new)
    return text


def make_models(owned):
    ascii_dir = os.path.join(vts_e2e.REPO, "models", ASCII)
    unicode_dir = os.path.join(vts_e2e.REPO, "models", UNICODE)
    os.mkdir(ascii_dir)
    owned.append(ascii_dir)
    vts_e2e.make_copy(ascii_dir, ASCII, SOURCE, {}, still=True)
    os.mkdir(unicode_dir)
    owned.append(unicode_dir)
    shutil.copytree(os.path.join(vts_e2e.SAMPLES, SOURCE), unicode_dir, dirs_exist_ok=True)
    for root, dirs, files in os.walk(unicode_dir, topdown=False):
        for name in files + dirs:
            os.rename(os.path.join(root, name), os.path.join(root, renamed(name)))
    path = os.path.join(unicode_dir, UNICODE + ".model3.json")
    with open(path, encoding="utf-8") as reader:
        model = json.loads(renamed(reader.read()))
    model["FileReferences"].pop("Motions", None)
    with open(path, "w", encoding="utf-8") as writer:
        json.dump(model, writer, ensure_ascii=False)
    leftovers = [name for _, dirs, files in os.walk(unicode_dir) for name in dirs + files if name.isascii()]
    bad_dir = os.path.join(vts_e2e.REPO, "models", bad)
    os.mkdir(bad_dir)
    owned.append(bad_dir)
    vts_e2e.make_copy(bad_dir, bad, bad_source, {}, still=True)
    with open(os.path.join(bad_dir, bad_source + ".moc3"), "r+b") as handle:
        handle.seek(bad_offset)
        handle.write(struct.pack("<4I", bad_word, bad_word, bad_word, bad_word))
    return leftovers


def settled_mean(port, name):
    vts_e2e.switch_model(port, name)
    status, images, _ = vts_e2e.idle_frames(port, SETTLE_FRAMES + FRAMES)
    return status, vts_e2e.mean_image(images[SETTLE_FRAMES:])


def run(args, result):
    dirs = []
    try:
        leftovers = make_models(dirs)
        result.add("a_all_files_renamed", (not leftovers, "ascii names left %s" % leftovers))
        handle = vts_e2e.start_server(args.binary, args.port, args.out + ".log", model=ASCII, settings={"idle_actions": []})
        try:
            status, body = vts_e2e.http_call(args.port, "GET", "/models")
            listed = body.decode("utf-8", errors="replace")
            result.add("b_unicode_model_listed", (status == 200 and UNICODE in listed, "status=%d %s" % (status, listed.strip())))
            ascii_status, ascii_mean = settled_mean(args.port, ASCII)
            unicode_status, unicode_mean = settled_mean(args.port, UNICODE)
            bad_status, bad_body = vts_e2e.http_call(args.port, "PUT", "/settings", json.dumps({"model": bad}).encode())
            models_status = vts_e2e.http_call(args.port, "GET", "/models")[0]
            alive = handle["proc"].poll() is None
            after_status, after_mean = settled_mean(args.port, ASCII)
        finally:
            vts_e2e.stop_server(handle)
    finally:
        for path in dirs:
            shutil.rmtree(path, ignore_errors=True)
    blank = vts_e2e.image_diff(ascii_mean, [0.0] * len(ascii_mean))
    diff = vts_e2e.image_diff(ascii_mean, unicode_mean)
    ok = ascii_status == 200 and unicode_status == 200 and blank > BLANK_FLOOR and len(unicode_mean) == len(ascii_mean) and diff <= SAME_LIMIT
    result.add("c_unicode_frame_matches_ascii", (ok, "statuses=%d,%d ascii_vs_black=%.3f need >%.1f unicode_vs_ascii=%.3f need <=%.1f" % (ascii_status, unicode_status, blank, BLANK_FLOOR, diff, SAME_LIMIT)))
    result.add("d_corrupt_moc_rejected", (bad_status // 100 in (4, 5) and models_status == 200 and alive, "put_status=%d body=%s models_status=%d alive=%s" % (bad_status, bad_body.decode(errors="replace").strip(), models_status, alive)))
    after_diff = vts_e2e.image_diff(ascii_mean, after_mean)
    after_ok = after_status == 200 and len(after_mean) == len(ascii_mean) and after_diff <= SAME_LIMIT
    result.add("e_good_model_renders_after", (after_ok, "status=%d after_vs_ascii=%.3f need <=%.1f" % (after_status, after_diff, SAME_LIMIT)))


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True)
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
