#!/usr/bin/env python3
"""Compare the grain that grained encodes add with the grain a denoiser removed.

    scripts/accuracy.py --source original.mkv --denoised denoised.mkv [--crop W:H:X:Y] grained.ivf ...

Every grained file must be the same encode of the denoised video, with a different
film grain table applied. Each one is decoded twice with dav1d, with and without grain,
and the difference is the grain the decoder adds. The removed grain is the source minus
the denoised video. Only flat areas of the denoised picture are compared, so detail the
denoiser removed does not count as grain.

The error is the mean difference in grain deviation for every stretch of --segment
frames and every brightness level, weighted by how many blocks each holds. Values are on
the 0 to 255 scale. Needs ffmpeg with libdav1d, ffprobe and numpy.
"""
import argparse
import subprocess

import numpy as np

BLOCK = 32
LEVELS = 16


def probe_size(path):
    out = subprocess.run(
        ["ffprobe", "-v", "error", "-select_streams", "V:0", "-show_entries", "stream=width,height", "-of", "csv=p=0", path],
        capture_output=True, text=True, check=True,
    ).stdout.strip()
    width, height = out.split(",")[:2]
    return int(width), int(height)


def reader(path, step, frame_bytes, grain=None, crop=None, pix_fmt="yuv420p16le"):
    command = ["ffmpeg", "-v", "error", "-nostdin", "-threads", "4"]
    if grain is not None:
        command += ["-c:v", "libdav1d", "-filmgrain", str(grain)]
    filters = f"select=not(mod(n\\,{step}))"
    if crop:
        filters = f"crop={crop}," + filters
    command += ["-i", path, "-map", "0:V:0", "-vf", filters, "-fps_mode", "passthrough", "-pix_fmt", pix_fmt, "-f", "rawvideo", "-"]
    return subprocess.Popen(command, stdout=subprocess.PIPE, bufsize=frame_bytes * 2)


def blocks(plane, size):
    h, w = plane.shape
    h -= h % size
    w -= w % size
    return plane[:h, :w].reshape(h // size, size, w // size, size).transpose(0, 2, 1, 3).reshape(h // size, w // size, size * size)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--source", required=True)
    parser.add_argument("--denoised", required=True)
    parser.add_argument("--crop", help="area of the source that the denoised video covers, as W:H:X:Y")
    parser.add_argument("--step", type=int, default=24, help="compare every Nth frame")
    parser.add_argument("--segment", type=int, default=240, help="frames per comparison stretch")
    parser.add_argument("--bit-depth", type=int, default=8, help="bit depth of the denoised video, sets the 0 to 255 scale")
    parser.add_argument("--luma-only", action="store_true", help="compare luma only, for 4:2:2, 4:4:4 and monochrome video")
    parser.add_argument("grained", nargs="+")
    args = parser.parse_args()

    width, height = probe_size(args.denoised)
    pix_fmt = "gray16le" if args.luma_only else "yuv420p16le"
    samples = width * height if args.luma_only else width * height * 3 // 2
    frame_bytes = samples * 2
    scale = float(1 << (16 - 8))

    def frame(proc):
        data = proc.stdout.read(frame_bytes)
        if len(data) < frame_bytes:
            return None
        values = np.frombuffer(data, "<u2").astype(np.float32) / scale
        luma = values[: width * height].reshape(height, width)
        if args.luma_only:
            empty = np.zeros((height // 2, width // 2), np.float32)
            return luma, empty, empty
        cb = values[width * height : width * height * 5 // 4].reshape(height // 2, width // 2)
        cr = values[width * height * 5 // 4 :].reshape(height // 2, width // 2)
        return luma, cb, cr

    source = reader(args.source, args.step, frame_bytes, crop=args.crop, pix_fmt=pix_fmt)
    denoised = reader(args.denoised, args.step, frame_bytes, pix_fmt=pix_fmt)
    grained = {
        name: (reader(name, args.step, frame_bytes, 1, pix_fmt=pix_fmt), reader(name, args.step, frame_bytes, 0, pix_fmt=pix_fmt))
        for name in args.grained
    }
    names = ["removed"] + args.grained
    stats = {}
    index = 0
    while True:
        s, d = frame(source), frame(denoised)
        if s is None or d is None:
            break
        stretch = index * args.step // args.segment
        index += 1
        den_blocks = blocks(d[0], BLOCK)
        flat = (den_blocks.std(axis=2) < 1.5) & ~(blocks(s[0], BLOCK).std(axis=2) == 0)
        level = np.clip((den_blocks.mean(axis=2) / LEVELS).astype(int), 0, 255 // LEVELS)
        grains = {"removed": (s[0] - d[0], s[1] - d[1], s[2] - d[2])}
        for name in args.grained:
            with_grain, without = frame(grained[name][0]), frame(grained[name][1])
            grains[name] = tuple(a - b for a, b in zip(with_grain, without))
        for name, (gy, gu, gv) in grains.items():
            entry = stats.setdefault((name, stretch), dict(var=np.zeros(16), count=np.zeros(16), corr=np.zeros(2), energy=0.0, chroma=0.0, chroma_count=0))
            gb = blocks(gy, BLOCK)
            gb = gb - gb.mean(axis=2, keepdims=True)
            np.add.at(entry["var"], level[flat], gb.var(axis=2)[flat])
            np.add.at(entry["count"], level[flat], 1)
            flat_blocks = gb[flat].reshape(-1, BLOCK, BLOCK)
            if len(flat_blocks):
                entry["energy"] += float((flat_blocks**2).sum())
                entry["corr"] += [
                    float((flat_blocks[:, :, 1:] * flat_blocks[:, :, :-1]).sum()) * BLOCK / (BLOCK - 1),
                    float((flat_blocks[:, 1:, :] * flat_blocks[:, :-1, :]).sum()) * BLOCK / (BLOCK - 1),
                ]
            for plane in (gu, gv):
                cb = blocks(plane, BLOCK // 2)
                cb = cb - cb.mean(axis=2, keepdims=True)
                entry["chroma"] += float(cb.var(axis=2)[flat].sum())
                entry["chroma_count"] += int(flat.sum())

    stretches = sorted({stretch for (_, stretch) in stats})
    print(f"frames compared: {index}, stretches: {len(stretches)}")
    print(f"{'file':32s} {'luma':>6s} {'chroma':>7s} {'corr h':>7s} {'corr v':>7s} {'error':>7s} {'relative':>9s}")
    for name in names:
        rows = [stats[(name, s)] for s in stretches]
        var = sum(r["var"] for r in rows)
        count = sum(r["count"] for r in rows)
        energy = sum(r["energy"] for r in rows)
        corr = sum(r["corr"] for r in rows) / max(energy, 1e-9)
        chroma = sum(r["chroma"] for r in rows) / max(sum(r["chroma_count"] for r in rows), 1)
        error = relative = total = 0.0
        for s in stretches:
            removed, mine = stats[("removed", s)], stats[(name, s)]
            used = removed["count"] > 0
            truth = np.sqrt(removed["var"][used] / removed["count"][used])
            measured = np.sqrt(mine["var"][used] / removed["count"][used])
            weight = removed["count"][used]
            error += (np.abs(measured - truth) * weight).sum()
            relative += (np.abs(measured - truth) / np.maximum(truth, 0.05) * weight).sum()
            total += weight.sum()
        print(f"{name[-32:]:32s} {np.sqrt(var.sum() / max(count.sum(), 1)):6.3f} {np.sqrt(chroma):7.3f} {corr[0]:7.2f} {corr[1]:7.2f} {error / max(total, 1):7.3f} {100 * relative / max(total, 1):8.1f}%")


if __name__ == "__main__":
    main()
