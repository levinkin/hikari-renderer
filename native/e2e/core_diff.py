import argparse
import ctypes
import glob
import json
import os
import random
import sys
import time

TRIALS = 40
SEED = 7
VERTEX_LIMIT = 1e-3


def load_core(path):
    core = ctypes.CDLL(path)
    core.csmGetVersion.restype = ctypes.c_uint
    core.csmReviveMocInPlace.restype = ctypes.c_void_p
    core.csmGetSizeofModel.restype = ctypes.c_uint
    core.csmInitializeModelInPlace.restype = ctypes.c_void_p
    core.csmGetDrawableCount.restype = ctypes.c_int
    core.csmGetParameterCount.restype = ctypes.c_int
    core.csmGetDrawableVertexCounts.restype = ctypes.POINTER(ctypes.c_int)
    core.csmGetDrawableVertexPositions.restype = ctypes.POINTER(ctypes.POINTER(ctypes.c_float))
    core.csmGetDrawableOpacities.restype = ctypes.POINTER(ctypes.c_float)
    core.csmGetDrawableDrawOrders.restype = ctypes.POINTER(ctypes.c_int)
    core.csmGetDrawableDynamicFlags.restype = ctypes.POINTER(ctypes.c_ubyte)
    core.csmGetParameterValues.restype = ctypes.POINTER(ctypes.c_float)
    core.csmGetParameterMinimumValues.restype = ctypes.POINTER(ctypes.c_float)
    core.csmGetParameterMaximumValues.restype = ctypes.POINTER(ctypes.c_float)
    return core


def load_model(core, path, buffers):
    data = open(path, "rb").read()
    moc_buffer = ctypes.create_string_buffer(len(data) + 64)
    buffers.append(moc_buffer)
    moc_address = (ctypes.addressof(moc_buffer) + 63) // 64 * 64
    ctypes.memmove(moc_address, data, len(data))
    moc = core.csmReviveMocInPlace(ctypes.c_void_p(moc_address), ctypes.c_uint(len(data)))
    if not moc:
        return None
    size = core.csmGetSizeofModel(ctypes.c_void_p(moc))
    model_buffer = ctypes.create_string_buffer(size + 16)
    buffers.append(model_buffer)
    model_address = (ctypes.addressof(model_buffer) + 15) // 16 * 16
    return ctypes.c_void_p(core.csmInitializeModelInPlace(ctypes.c_void_p(moc), ctypes.c_void_p(model_address), ctypes.c_uint(size)))


def compare_model(reference, candidate, path):
    buffers = []
    reference_model = load_model(reference, path, buffers)
    candidate_model = load_model(candidate, path, buffers)
    row = {"model": os.path.basename(path), "loaded": reference_model is not None and candidate_model is not None}
    if not row["loaded"]:
        return row
    drawables = reference.csmGetDrawableCount(reference_model)
    params = reference.csmGetParameterCount(reference_model)
    row["drawables"] = drawables
    row["parameters"] = params
    row["drawable_count_match"] = drawables == candidate.csmGetDrawableCount(candidate_model)
    mins = reference.csmGetParameterMinimumValues(reference_model)
    maxs = reference.csmGetParameterMaximumValues(reference_model)
    generator = random.Random(SEED)
    vertex_diff = 0.0
    opacity_diff = 0.0
    order_mismatches = 0
    flag_mismatches = 0
    reference_seconds = 0.0
    candidate_seconds = 0.0
    for trial in range(TRIALS):
        if trial > 0:
            values = [mins[i] + (maxs[i] - mins[i]) * generator.random() for i in range(params)]
            reference_values = reference.csmGetParameterValues(reference_model)
            candidate_values = candidate.csmGetParameterValues(candidate_model)
            for index, value in enumerate(values):
                reference_values[index] = value
                candidate_values[index] = value
        started = time.perf_counter()
        reference.csmUpdateModel(reference_model)
        reference_seconds += time.perf_counter() - started
        started = time.perf_counter()
        candidate.csmUpdateModel(candidate_model)
        candidate_seconds += time.perf_counter() - started
        counts = reference.csmGetDrawableVertexCounts(reference_model)
        reference_positions = reference.csmGetDrawableVertexPositions(reference_model)
        candidate_positions = candidate.csmGetDrawableVertexPositions(candidate_model)
        reference_opacities = reference.csmGetDrawableOpacities(reference_model)
        candidate_opacities = candidate.csmGetDrawableOpacities(candidate_model)
        reference_orders = reference.csmGetDrawableDrawOrders(reference_model)
        candidate_orders = candidate.csmGetDrawableDrawOrders(candidate_model)
        reference_flags = reference.csmGetDrawableDynamicFlags(reference_model)
        candidate_flags = candidate.csmGetDrawableDynamicFlags(candidate_model)
        for drawable in range(drawables):
            opacity_diff = max(opacity_diff, abs(reference_opacities[drawable] - candidate_opacities[drawable]))
            order_mismatches += reference_orders[drawable] != candidate_orders[drawable]
            flag_mismatches += (reference_flags[drawable] & 1) != (candidate_flags[drawable] & 1)
            for component in range(counts[drawable] * 2):
                vertex_diff = max(vertex_diff, abs(reference_positions[drawable][component] - candidate_positions[drawable][component]))
    row["max_vertex_diff"] = vertex_diff
    row["max_opacity_diff"] = opacity_diff
    row["draw_order_mismatches"] = order_mismatches
    row["visibility_mismatches"] = flag_mismatches
    row["update_ms_reference"] = reference_seconds / TRIALS * 1000
    row["update_ms_candidate"] = candidate_seconds / TRIALS * 1000
    row["pass"] = row["drawable_count_match"] and vertex_diff <= VERTEX_LIMIT and opacity_diff <= VERTEX_LIMIT and order_mismatches == 0 and flag_mismatches == 0
    return row


def main():
    parser = argparse.ArgumentParser(description="compare a candidate cubism core library against live2d's on real models")
    parser.add_argument("--reference", required=True)
    parser.add_argument("--candidate", required=True)
    parser.add_argument("--models", nargs="+", required=True)
    parser.add_argument("--out", required=True)
    args = parser.parse_args()
    reference = load_core(args.reference)
    candidate = load_core(args.candidate)
    paths = []
    for root in args.models:
        paths.extend(sorted(glob.glob(os.path.join(root, "**", "*.moc3"), recursive=True)))
    result = {"reference_version": hex(reference.csmGetVersion()), "candidate_version": hex(candidate.csmGetVersion()), "trials": TRIALS, "seed": SEED, "rows": []}
    try:
        for path in paths:
            row = compare_model(reference, candidate, path)
            result["rows"].append(row)
            print("PASS" if row.get("pass") else "FAIL", json.dumps(row))
    finally:
        with open(args.out, "w") as handle:
            json.dump(result, handle, indent=1)
    return 0 if result["rows"] and all(row.get("pass") for row in result["rows"]) else 1


if __name__ == "__main__":
    sys.exit(main())
