#!/usr/bin/env python3
import argparse
import glob
import json
import os
import shutil
import signal
import statistics
import struct
import subprocess
import sys
import tempfile
import time
import zlib

import vts_e2e
import window_e2e

REPO = window_e2e.REPO
PORTS = range(9326, 9330)
BIG = (3840, 2160)
ROBOT_LANES = [("robot", "rgb565", "160x120"), ("hires", "jpeg", "960x720")]
ROBOT_SIZES = {"robot": (160, 120), "hires": (960, 720)}
ROBOT_FPS = 24
ROBOT_FRAMES = 24
PULL_SECONDS = 20
TOOL_FRAMES = 30
TOOL_WARMUP = 3
TILE = 320
EDGE = 24
STAIR_SLACK = 0.5
LIT_PIXELS = 1000
TOOL_SOURCE = r"""
#import <Foundation/Foundation.h>
#import <Metal/Metal.h>
#include <mach/mach.h>
#include <cstdio>
#include <cstdlib>
#include <algorithm>
#include <chrono>
#include <vector>
#include "engine.h"

static void keep_eyes_open(void* context)
{
    engine* instance = (engine*)context;
    const char* param_names[] = {"ParamEyeLOpen", "ParamEyeROpen"};
    for (const char* param_name : param_names)
    {
        if (engine_has_param(instance, param_name))
        {
            engine_param_set(instance, param_name, engine_param_default(instance, param_name));
        }
    }
}

int main(int argc, char** argv)
{
    @autoreleasepool
    {
        if (argc != 9)
        {
            fprintf(stderr, "usage: model w h zoom x y frames out.rgba\n");
            return 2;
        }
        srand(1);
        engine_core_load();
        int width = atoi(argv[2]);
        int height = atoi(argv[3]);
        engine* instance = engine_create(argv[1], width, height);
        if (instance == NULL)
        {
            fprintf(stderr, "engine_create failed\n");
            return 1;
        }
        engine_breath_enable(instance, false);
        engine_idle_motions_enable(instance, false);
        engine_expressions_stop(instance);
        engine_view_set(instance, atof(argv[4]), atof(argv[5]), atof(argv[6]), 0);
        int frames = atoi(argv[7]);
        std::vector<double> frame_times;
        const uint8_t* pixels = NULL;
        for (int frame = 0; frame < frames; frame++)
        {
            auto start = std::chrono::steady_clock::now();
            engine_update(instance, 1000.0f / 30.0f, NULL, keep_eyes_open, instance);
            pixels = engine_render(instance, 0);
            frame_times.push_back(std::chrono::duration<double, std::milli>(std::chrono::steady_clock::now() - start).count());
        }
        FILE* output = fopen(argv[8], "wb");
        fwrite(pixels, 1, (size_t)width * height * 4, output);
        fclose(output);
        task_vm_info_data_t memory_info = {};
        mach_msg_type_number_t count = TASK_VM_INFO_COUNT;
        task_info(mach_task_self(), TASK_VM_INFO, (task_info_t)&memory_info, &count);
        id<MTLDevice> device = MTLCreateSystemDefaultDevice();
        printf("{\"gpu\": \"%s\", \"footprint_mb\": %.1f, \"footprint_peak_mb\": %.1f, \"gpu_allocated_mb\": %.1f, \"ms\": [", engine_gpu_name(instance), memory_info.phys_footprint / 1048576.0, memory_info.ledger_phys_footprint_peak / 1048576.0, [device currentAllocatedSize] / 1048576.0);
        for (size_t index = 0; index < frame_times.size(); index++)
        {
            printf("%s%.3f", index ? ", " : "", frame_times[index]);
        }
        printf("]}\n");
        engine_destroy(instance);
    }
    return 0;
}
"""


def find_model(name):
    for root in [os.path.join(REPO, "models"), window_e2e.SAMPLES]:
        path = os.path.join(root, name, name + ".model3.json")
        if os.path.exists(path):
            return path
    raise RuntimeError("no %s.model3.json under models/ or sdk samples" % name)


def engine_libs(binary):
    here = os.path.dirname(os.path.realpath(binary))
    archives = sorted(glob.glob(os.path.join(here, "libhikari_engine.a")) or glob.glob(os.path.join(here, "build", "hikari-*", "out", "libhikari_engine.a")), key=os.path.getmtime)
    if not archives:
        raise RuntimeError("no libhikari_engine.a next to %s or in its build/hikari-*/out; copy it and libcubism_framework.a beside the binary" % binary)
    archive_dir = os.path.dirname(archives[-1])
    return [os.path.join(archive_dir, "libhikari_engine.a"), os.path.join(archive_dir, "libcubism_framework.a")]


def core_dylib(binary):
    here = os.path.dirname(os.path.realpath(binary))
    for path in [os.path.join(here, "libLive2DCubismCore.dylib"), os.path.join(here, "core", "libLive2DCubismCore.dylib")]:
        if os.path.exists(path):
            return path
    raise RuntimeError("no libLive2DCubismCore.dylib next to %s" % binary)


def stage(binary, side_dir):
    os.makedirs(side_dir, exist_ok=True)
    source = os.path.join(side_dir, "mask_tool.mm")
    with open(source, "w") as handle:
        handle.write(TOOL_SOURCE)
    tool_path = os.path.join(side_dir, "mask_tool")
    engine_dir = os.path.join(REPO, "native", "engine")
    subprocess.run(["clang++", "-O2", "-std=c++17", "-fobjc-arc", "-I" + engine_dir, source] + engine_libs(binary) + ["-framework", "Metal", "-framework", "Foundation", "-framework", "ImageIO", "-framework", "CoreGraphics", "-framework", "AppKit", "-framework", "QuartzCore", "-lc++", "-o", tool_path], check=True, capture_output=True, timeout=300)
    shutil.copy2(core_dylib(binary), side_dir)
    server = os.path.join(side_dir, "hikari")
    shutil.copy2(binary, server)
    return tool_path, server


def render(tool_path, model_path, size, view, out_path):
    done = subprocess.run([tool_path, model_path, str(size[0]), str(size[1])] + ["%g" % value for value in view] + [str(TOOL_WARMUP + TOOL_FRAMES), out_path], capture_output=True, text=True, timeout=300)
    if done.returncode != 0:
        raise RuntimeError("mask_tool failed %d: %s" % (done.returncode, done.stderr.strip()))
    stats = json.loads(done.stdout.strip().splitlines()[-1])
    timed = sorted(stats.pop("ms")[TOOL_WARMUP:])
    stats["frame_ms_median"] = round(statistics.median(timed), 3)
    stats["frame_ms_p90"] = round(timed[int(len(timed) * 0.9)], 3)
    with open(out_path, "rb") as reader:
        return reader.read(), stats


def downsample(pixels, width, height):
    output = bytearray()
    for row in range(0, height, 2):
        top = pixels[row * width * 4:(row + 1) * width * 4]
        bottom = pixels[(row + 1) * width * 4:(row + 2) * width * 4]
        for column in range(0, width * 4, 8):
            for channel in range(4):
                output.append((top[column + channel] + top[column + 4 + channel] + bottom[column + channel] + bottom[column + 4 + channel] + 2) // 4)
    return bytes(output)


def pick_tile(main, branch, reference, width, height):
    best = (-1, -1, (0, 0, TILE, TILE))
    for top in range(0, height - TILE + 1, TILE // 2):
        for left in range(0, width - TILE + 1, TILE // 2):
            main_tile, branch_tile, reference_tile = (b"".join(image[((top + row) * width + left) * 4:((top + row) * width + left + TILE) * 4] for row in range(TILE)) for image in (main, branch, reference))
            best = max(best, (sum(abs(main_value - branch_value) for main_value, branch_value in zip(main_tile, branch_tile)), staircase(main_tile, reference_tile, TILE, TILE)[0], (left, top, TILE, TILE)))
    return best[2], "main_branch_diff=%d" % best[0] if best[0] else "main==branch, picked the tile with the highest main staircase score"


def staircase(image, reference, width, height):
    total = 0
    edges = 0
    for row in range(height - 1):
        for column in range(width - 1):
            offset = (row * width + column) * 4
            luma = reference[offset] + reference[offset + 1] + reference[offset + 2]
            gradient = abs(luma - reference[offset + 4] - reference[offset + 5] - reference[offset + 6]) + abs(luma - reference[offset + width * 4] - reference[offset + width * 4 + 1] - reference[offset + width * 4 + 2])
            if gradient > EDGE * 3:
                edges += 1
                for neighbor in (offset, offset + 4, offset + width * 4):
                    total += abs(image[neighbor] - reference[neighbor]) + abs(image[neighbor + 1] - reference[neighbor + 1]) + abs(image[neighbor + 2] - reference[neighbor + 2])
    return (total / (edges * 9) if edges else 0.0), edges


def png(path, pixels, width, height):
    raw = b"".join(b"\x00" + pixels[row * width * 4:(row + 1) * width * 4] for row in range(height))
    chunks = [(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0)), (b"IDAT", zlib.compress(raw, 6)), (b"IEND", b"")]
    with open(path, "wb") as handle:
        handle.write(b"\x89PNG\r\n\x1a\n" + b"".join(struct.pack(">I", len(body)) + kind + body + struct.pack(">I", zlib.crc32(kind + body)) for kind, body in chunks))


def side_by_side(images, width, height):
    opaque = [bytes(value if index % 4 != 3 else 255 for index, value in enumerate(image)) for image in images]
    return b"".join(b"".join(image[row * width * 4:(row + 1) * width * 4] for image in opaque) for row in range(height)), width * len(images)


def bmp_rgb(data):
    offset, width, height, bits = struct.unpack_from("<I", data, 10)[0], struct.unpack_from("<i", data, 18)[0], struct.unpack_from("<i", data, 22)[0], struct.unpack_from("<H", data, 28)[0]
    step = bits // 8
    stride = (width * step + 3) & ~3
    output = bytearray()
    for row in range(abs(height)):
        start = offset + (row if height < 0 else abs(height) - 1 - row) * stride
        for column in range(width):
            blue, green, red = data[start + column * step:start + column * step + 3]
            output += bytes((red, green, blue))
    return bytes(output)


def jpeg_rgb(data, scratch):
    with open(scratch + ".jpg", "wb") as handle:
        handle.write(data)
    window_e2e.tool(["sips", "-s", "format", "bmp", scratch + ".jpg", "--out", scratch + ".bmp"])
    with open(scratch + ".bmp", "rb") as reader:
        return bmp_rgb(reader.read())


def footprint_mb(pid):
    text = window_e2e.tool(["footprint", "-p", str(pid)])
    found = {}
    for line in text.splitlines():
        parts = line.split()
        if len(parts) >= 3 and parts[0] in ("phys_footprint:", "phys_footprint_peak:"):
            found[parts[0][:-1]] = float(parts[1]) * {"B": 1 / 1048576, "KB": 1 / 1024, "MB": 1, "GB": 1024}[parts[2]]
    return found


def robot_frames(server, model, side_dir):
    port = window_e2e.free_port()
    handle = vts_e2e.start_server(server, port, os.path.join(side_dir, "server.log"), model=model)
    lanes = {}
    try:
        for lane, codec, size in ROBOT_LANES:
            status, frames, _, _ = vts_e2e.read_frames(port, "GET", "/listen?fps=%d&codec=%s&size=%s" % (ROBOT_FPS, codec, size), ROBOT_FRAMES)
            if status != 200 or len(frames) < ROBOT_FRAMES:
                raise RuntimeError("%s lane status=%d frames=%d" % (lane, status, len(frames)))
            images = [vts_e2e.decode_rgb565(frame) if codec == "rgb565" else jpeg_rgb(frame, os.path.join(side_dir, "frame")) for frame in frames]
            lanes[lane] = {"images": images, "memory": footprint_mb(handle["proc"].pid)}
    finally:
        vts_e2e.stop_server(handle)
    return lanes


def lane_mae(main_images, branch_images):
    frame_errors = []
    peak = 0
    for main_image, branch_image in zip(main_images, branch_images):
        differences = [abs(main_value - branch_value) for main_value, branch_value in zip(main_image, branch_image)]
        frame_errors.append(sum(differences) / len(differences))
        peak = max(peak, max(differences))
    return {"mae_mean": round(statistics.mean(frame_errors), 4), "mae_worst_frame": round(max(frame_errors), 4), "max": peak, "frames": len(frame_errors)}


def run(args, result, scratch):
    model_path = find_model(args.model)
    view = (args.zoom, args.x, args.y)
    near = (args.zoom * 2, args.x * 2, args.y * 2)
    sides = {}
    for side, binary in (("main", args.main_bin), ("branch", args.branch_bin)):
        side_dir = os.path.join(scratch, side)
        tool_path, server = stage(binary, side_dir)
        pixels, stats = render(tool_path, model_path, BIG, view, os.path.join(side_dir, "big.rgba"))
        lit = sum(1 for index in range(0, len(pixels), 64) if pixels[index] + pixels[index + 1] + pixels[index + 2] > 60) * 16
        result.add("4k_%s" % side, (lit >= LIT_PIXELS, "lit=%d %s" % (lit, stats)))
        robot = {}
        for lane, size in ROBOT_SIZES.items():
            robot[lane] = render(tool_path, model_path, size, (1, 0, 0), os.path.join(side_dir, lane + ".rgba"))[1]
        sides[side] = {"tool": tool_path, "server": server, "big": pixels, "stats": stats, "robot_tool": robot}
    reference_full = render(sides["branch"]["tool"], model_path, BIG, near, os.path.join(scratch, "branch", "near.rgba"))[0]
    half = (BIG[0] // 2, BIG[1] // 2)
    reference = downsample(reference_full, BIG[0], BIG[1])
    main_half, branch_half = (b"".join(sides[side]["big"][((BIG[1] // 4 + row) * BIG[0] + BIG[0] // 4) * 4:((BIG[1] // 4 + row) * BIG[0] + BIG[0] // 4 + half[0]) * 4] for row in range(half[1])) for side in ("main", "branch"))
    if args.crop:
        box = tuple(int(value) for value in args.crop.split(","))
        how = "--crop, in center-half coordinates"
    else:
        box, how = pick_tile(main_half, branch_half, reference, half[0], half[1])
    crops = [b"".join(image[((box[1] + row) * half[0] + box[0]) * 4:((box[1] + row) * half[0] + box[0] + box[2]) * 4] for row in range(box[3])) for image in (main_half, branch_half, reference)]
    main_score, edges = staircase(crops[0], crops[2], box[2], box[3])
    branch_score = staircase(crops[1], crops[2], box[2], box[3])[0]
    joined, joined_width = side_by_side(crops, box[2], box[3])
    png(os.path.join(args.out, "crop_main_branch_ref.png"), joined, joined_width, box[3])
    png(os.path.join(args.out, "main_4k.png"), sides["main"]["big"], BIG[0], BIG[1])
    result.payload["crop"] = {"box_center_half": box, "box_4k": (box[0] + BIG[0] // 4, box[1] + BIG[1] // 4, box[2], box[3]), "how": how}
    result.payload["staircase"] = {"main": round(main_score, 3), "branch": round(branch_score, 3), "edge_pixels": edges}
    result.add("staircase", (edges > 0 and branch_score <= main_score + STAIR_SLACK, "main=%.3f branch=%.3f edge_px=%d box=%s (%s)" % (main_score, branch_score, edges, box, how)))
    for side in sides:
        sides[side]["lanes"] = robot_frames(sides[side]["server"], args.model, os.path.join(scratch, side))
    for lane, _, _ in ROBOT_LANES:
        size = ROBOT_SIZES[lane]
        stats = lane_mae(sides["main"]["lanes"][lane]["images"], sides["branch"]["lanes"][lane]["images"])
        result.payload.setdefault("robot", {})[lane] = stats
        for side in sides:
            last = sides[side]["lanes"][lane]["images"][-1]
            png(os.path.join(args.out, "%s_%s.png" % (lane, side)), b"".join(last[index:index + 3] + b"\xff" for index in range(0, len(last), 3)), size[0], size[1])
        result.add("robot_%s" % lane, (stats["frames"] == ROBOT_FRAMES, "%s size=%dx%d" % (stats, size[0], size[1])))
    result.payload["memory"] = {side: {"4k_tool": sides[side]["stats"], "robot_tool": sides[side]["robot_tool"], "server": {lane: sides[side]["lanes"][lane]["memory"] for lane in sides[side]["lanes"]}} for side in sides}
    result.flush()


def selftest(result):
    flat = bytes([40, 40, 40, 255]) * 64
    step = b"".join((bytes([200, 200, 200, 255]) if column < 4 else bytes([40, 40, 40, 255])) for row in range(8) for column in range(8))
    stair = b"".join((bytes([200, 200, 200, 255]) if column < 4 + (row // 4) else bytes([40, 40, 40, 255])) for row in range(8) for column in range(8))
    result.add("staircase_zero_on_match", (staircase(step, step, 8, 8) == (0.0, 7), "same image scores 0 on 7 edge pixels"))
    result.add("staircase_counts_misplaced_edge", (staircase(stair, step, 8, 8)[0] > 0, "shifted edge scores above 0"))
    result.add("staircase_flat_has_no_edges", (staircase(flat, flat, 8, 8) == (0.0, 0), "flat ref has no edges"))
    result.add("downsample_box", (downsample(bytes([0, 0, 0, 0, 4, 4, 4, 4, 8, 8, 8, 8, 12, 12, 12, 12]), 2, 2) == bytes([6, 6, 6, 6]), "2x2 mean rounds"))
    bmp = b"BM" + struct.pack("<IHHI", 54 + 8, 0, 0, 54) + struct.pack("<IiiHHIIiiII", 40, 1, 2, 1, 24, 0, 8, 0, 0, 0, 0) + bytes([1, 2, 3, 0, 4, 5, 6, 0])
    result.add("bmp_bottom_up_24", (bmp_rgb(bmp) == bytes([6, 5, 4, 3, 2, 1]), "rows flipped, bgr to rgb, padding skipped"))
    result.add("lane_mae", (lane_mae([bytes([0, 10])], [bytes([2, 10])]) == {"mae_mean": 1.0, "mae_worst_frame": 1.0, "max": 2, "frames": 1}, "mean and max"))


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("model", nargs="?")
    parser.add_argument("--main-bin")
    parser.add_argument("--branch-bin")
    parser.add_argument("--out")
    parser.add_argument("--zoom", type=float, default=3.0)
    parser.add_argument("--x", type=float, default=0.0)
    parser.add_argument("--y", type=float, default=-0.2)
    parser.add_argument("--crop", help="x,y,w,h inside the centre half (1920x1080) of the 4k frame")
    parser.add_argument("--selftest", action="store_true")
    args = parser.parse_args()
    if not args.selftest and not (args.model and args.main_bin and args.branch_bin and args.out):
        parser.error("model, --main-bin, --branch-bin and --out are required without --selftest")
    window_e2e.PORTS = PORTS
    vts_e2e.TIMEOUT = PULL_SECONDS
    if args.out:
        os.makedirs(args.out, exist_ok=True)
    result = vts_e2e.Result(args.out and os.path.join(args.out, "result.json"))
    started = time.time()
    signal.signal(signal.SIGTERM, signal.default_int_handler)
    scratch = tempfile.TemporaryDirectory()
    try:
        if args.selftest:
            selftest(result)
        else:
            run(args, result, scratch.name)
    except BaseException as error:
        result.payload["error"] = "%s: %s" % (type(error).__name__, error)
        result.flush()
        raise
    finally:
        scratch.cleanup()
    result.payload["seconds"] = round(time.time() - started, 1)
    result.flush()
    return vts_e2e.finish(result)


if __name__ == "__main__":
    sys.exit(main())
