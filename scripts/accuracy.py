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

Two more columns look at what a viewer sees. "vs source" compares the noise in the
grained decode with the noise in the source, in the same flat areas and after taking out
gradients, the same way as the error. It also counts grain the encoder smoothed away, so
the row for the encode without grain shows how much is missing before any grain is
added. "spill" is the grain the decoder adds where the source is perfectly constant, such
as letterbox bars and black frames, and should be 0.
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


_ramp = np.arange(BLOCK, dtype=np.float32) - (BLOCK - 1) / 2
RAMP_X = np.tile(_ramp, BLOCK)
RAMP_Y = np.repeat(_ramp, BLOCK)


def detrended_variance(block_values):
    """Variance of every block after taking out its mean and a linear gradient."""
    centred = block_values - block_values.mean(axis=-1, keepdims=True)
    gx = (centred @ RAMP_X) / (RAMP_X @ RAMP_X)
    gy = (centred @ RAMP_Y) / (RAMP_Y @ RAMP_Y)
    rest = centred - gx[..., None] * RAMP_X - gy[..., None] * RAMP_Y
    return rest.var(axis=-1)


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
    bare = "encode, no grain"
    stats = {}
    # Noise the viewer sees in flat areas, by stretch: variance and block count per level.
    views = {}
    # Grain added where the source is constant: summed deviation and block count.
    spill = {name: [0.0, 0] for name in args.grained}
    index = 0
    while True:
        s, d = frame(source), frame(denoised)
        if s is None or d is None:
            break
        stretch = index * args.step // args.segment
        index += 1
        den_blocks = blocks(d[0], BLOCK)
        src_blocks = blocks(s[0], BLOCK)
        constant = src_blocks.std(axis=2) == 0
        flat = (den_blocks.std(axis=2) < 1.5) & ~constant
        level = np.clip((den_blocks.mean(axis=2) / LEVELS).astype(int), 0, 255 // LEVELS)

        def view(name, picture):
            entry = views.setdefault((name, stretch), (np.zeros(16), np.zeros(16)))
            np.add.at(entry[0], level[flat], detrended_variance(blocks(picture, BLOCK)[flat]))
            np.add.at(entry[1], level[flat], 1)

        view("source", s[0])
        grains = {"removed": (s[0] - d[0], s[1] - d[1], s[2] - d[2])}
        for number, name in enumerate(args.grained):
            with_grain, without = frame(grained[name][0]), frame(grained[name][1])
            grains[name] = tuple(a - b for a, b in zip(with_grain, without))
            view(name, with_grain[0])
            if number == 0:
                view(bare, without[0])
            added = blocks(grains[name][0], BLOCK)[constant]
            spill[name][0] += float(added.std(axis=1).sum())
            spill[name][1] += len(added)
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
    def versus_source(name):
        error = total = 0.0
        for s in stretches:
            truth_var, count = views[("source", s)]
            used = count > 0
            truth = np.sqrt(truth_var[used] / count[used])
            seen = np.sqrt(views[(name, s)][0][used] / count[used])
            error += (np.abs(seen - truth) / np.maximum(truth, 0.05) * count[used]).sum()
            total += count[used].sum()
        return f"{100 * error / max(total, 1):9.1f}%"

    constant_blocks = spill[args.grained[0]][1]
    print(f"constant source blocks for spill: {constant_blocks}")
    print(
        f"{'file':32s} {'luma':>6s} {'chroma':>7s} {'corr h':>7s} {'corr v':>7s} {'error':>7s} {'relative':>9s}"
        f" {'vs source':>10s} {'spill':>7s}"
    )
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
        if name == "removed":
            extra = f" {'-':>10s} {'-':>7s}"
        else:
            spilled = spill[name][0] / spill[name][1] if spill[name][1] else 0.0
            extra = f" {versus_source(name)} {spilled:7.3f}"
        print(
            f"{name[-32:]:32s} {np.sqrt(var.sum() / max(count.sum(), 1)):6.3f} {np.sqrt(chroma):7.3f} {corr[0]:7.2f}"
            f" {corr[1]:7.2f} {error / max(total, 1):7.3f} {100 * relative / max(total, 1):8.1f}%{extra}"
        )
    print(f"{bare:32s} {'':>6s} {'':>7s} {'':>7s} {'':>7s} {'':>7s} {'':>9s} {versus_source(bare)} {'-':>7s}")


if __name__ == "__main__":
    main()
