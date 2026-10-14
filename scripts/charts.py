#!/usr/bin/env python3
"""Draw the benchmark charts of the README as SVG files in docs/.

    scripts/charts.py

The numbers are written below by hand from the output of scripts/accuracy.py and
scripts/speed.sh. Every chart follows the light or dark theme of the page it is shown on.
"""
import pathlib
import re

WIDTH = 800
LABEL_RIGHT = 236
STYLE = """<style>
text{font-family:-apple-system,BlinkMacSystemFont,"Segoe UI","Noto Sans",Helvetica,Arial,sans-serif}
.t{fill:#0b0b0b;font-size:15px;font-weight:600}
.l{fill:#0b0b0b;font-size:14px}
.n{fill:#52514e;font-size:12px}
.m{fill:#52514e;font-size:13px;font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace}
.v{fill:#0b0b0b;font-size:14px;font-weight:600}
.w{fill:#52514e;font-size:13px;font-weight:400}
.a{fill:#8a8984;font-size:12px}
.g{stroke:#e4e3df;stroke-width:1}
.b0{stroke:#8a8984;stroke-width:1}
.regrain{fill:#2a78d6}.regrain2{fill:#8bb8ec}.other{fill:#a3a29c}.other2{fill:#d3d2cc}
@media (prefers-color-scheme:dark){
.t,.l,.v{fill:#f0efec}.m,.n,.w{fill:#c3c2b7}.a{fill:#8f8e86}
.g{stroke:#30363d}.b0{stroke:#8f8e86}
.regrain{fill:#3987e5}.regrain2{fill:#1f5aa6}.other{fill:#8a8984}.other2{fill:#55544f}}
</style>"""


def esc(s):
    return s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


def label(name, flag=None):
    """A tool name, with an optional command line flag in monospace."""
    return esc(name) + (f' <tspan class="m">{esc(flag)}</tspan>' if flag else "")


def bar(out, cls, x, y, w, h):
    """A bar anchored on the axis with rounded ends on the data side."""
    w = max(w, 4)
    r = min(4, w, h / 2)
    out.append(
        f'<path class="{cls}" d="M{x},{y:.1f} h{w - r:.1f} a{r},{r} 0 0 1 {r},{r} '
        f"v{h - 2 * r:.1f} a{r},{r} 0 0 1 -{r},{r} h-{w - r:.1f} z\"/>"
    )


def value(out, x, y, text, note=None):
    note = f' <tspan class="w">{esc(note)}</tspan>' if note else ""
    out.append(f'<text class="v" x="{x:.1f}" y="{y:.1f}">{esc(text)}{note}</text>')


def panel(out, y, p):
    """Draw one panel from y and return the y below it."""
    right = p.get("right", 700)
    k = (right - LABEL_RIGHT) / p["max"]
    out.append(f'<text class="t" x="16" y="{y + 20}">{esc(p["title"])}</text>')
    y += 34
    if p.get("note"):
        out.append(f'<text class="n" x="16" y="{y + 4}">{esc(p["note"])}</text>')
        y += 20
    legend = p.get("legend")
    if legend:
        x = 16
        for cls, text in legend:
            out.append(f'<rect class="{cls}" x="{x}" y="{y + 1}" width="12" height="12" rx="3"/>')
            out.append(f'<text class="l" x="{x + 18}" y="{y + 12}">{text}</text>')
            x += 42 + len(re.sub("<[^>]*>", "", text)) * 7.2
        y += 26
    top = y + 6
    marks = []
    for row in p["rows"]:
        bars = row["bars"]
        if len(bars) == 1:
            h, gap = 26, 0
        else:
            h, gap = 13, 3
        block = len(bars) * h + (len(bars) - 1) * gap
        pad = 8 if len(bars) == 1 else 12
        y0 = y + pad
        mid = y0 + block / 2
        if row.get("sub"):
            marks.append(f'<text class="l" x="{LABEL_RIGHT - 14}" y="{mid - 3:.1f}" text-anchor="end">{row["label"]}</text>')
            marks.append(f'<text class="n" x="{LABEL_RIGHT - 14}" y="{mid + 13:.1f}" text-anchor="end">{esc(row["sub"])}</text>')
        else:
            marks.append(f'<text class="l" x="{LABEL_RIGHT - 14}" y="{mid + 5:.1f}" text-anchor="end">{row["label"]}</text>')
        by = y0
        for cls, v, text, *note in bars:
            w = max(v * k, 4)
            bar(marks, cls, LABEL_RIGHT, by, w, h)
            fy = by + h / 2 + (5 if h > 20 else 4.5)
            value(marks, LABEL_RIGHT + w + 8, fy, text, note[0] if note else None)
            by += h + gap
        y = y0 + block + pad
    bottom = y + 2
    for t, text in p["ticks"]:
        x = LABEL_RIGHT + t * k
        out.append(f'<line class="g" x1="{x:.1f}" y1="{top}" x2="{x:.1f}" y2="{bottom}"/>')
        out.append(f'<text class="a" x="{x:.1f}" y="{bottom + 18}" text-anchor="middle">{esc(text)}</text>')
    out.extend(marks)
    out.append(f'<line class="b0" x1="{LABEL_RIGHT}" y1="{top}" x2="{LABEL_RIGHT}" y2="{bottom}"/>')
    return bottom + 30


def chart(path, alt, panels):
    out = []
    y = 4
    for i, p in enumerate(panels):
        if i:
            y += 18
        y = panel(out, y, p)
    height = int(y + 4)
    head = (
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{WIDTH}" height="{height}" '
        f'viewBox="0 0 {WIDTH} {height}" role="img" aria-label="{esc(alt)}">'
    )
    path.write_text("\n".join([head, STYLE, *out, "</svg>"]) + "\n")


def percent_ticks(top, step):
    return [(t, f"{t}%") for t in range(0, top + 1, step)]


def error_rows(rows):
    return [
        {"label": label(*name), "bars": [(cls, rel, f"{rel}%", f"{err:.3f}")]}
        for name, cls, err, rel in rows
    ]


def main():
    docs = pathlib.Path(__file__).resolve().parent.parent / "docs"
    docs.mkdir(exist_ok=True)

    episode = [
        (("regrain", "--scan full"), "regrain", 0.021, 2.2),
        (("regrain", "--scan half"), "regrain", 0.023, 2.5),
        (("regrain", "--scan quarter"), "regrain", 0.036, 4.1),
        (("regrain", "--scan eighth"), "regrain", 0.039, 4.5),
        (("grav1synth 0.3.8",), "other", 0.080, 9.4),
    ]
    chart(
        docs / "accuracy-episode.svg",
        "Relative error on the whole episode, lower is better. "
        + ", ".join(f"{' '.join(n)} {r}% ({e:.3f})" for n, _, e, r in episode),
        [{
            "title": "Relative error on the whole episode, lower is better",
            "note": "Relative error in bold, then the error on the 0 to 255 scale",
            "rows": error_rows(episode),
            "max": 10, "ticks": percent_ticks(10, 2), "right": 680,
        }],
    )

    stretch = [
        (("regrain", "--scan full"), "regrain", 0.014, 1.7),
        (("regrain", "--scan quarter"), "regrain", 0.025, 2.7),
        (("grav1synth 0.3.8",), "other", 0.083, 14.2),
        (("grav1synth 0.2.0",), "other", 0.081, 13.7),
    ]
    chart(
        docs / "accuracy-stretch.svg",
        "Relative error from 10:00 to 12:00, lower is better. "
        + ", ".join(f"{' '.join(n)} {r}% ({e:.3f})" for n, _, e, r in stretch),
        [{
            "title": "Relative error from 10:00 to 12:00, lower is better",
            "note": "Relative error in bold, then the error on the 0 to 255 scale",
            "rows": error_rows(stretch),
            "max": 15, "ticks": percent_ticks(15, 5), "right": 680,
        }],
    )

    tools = [
        ("regrain", label("regrain", "full")),
        ("regrain2", label("regrain", "quarter")),
        ("other", "grav1synth 0.3.8"),
        ("other2", "grav1synth 0.2.0"),
    ]
    clips = [
        ("crowd_run", "Film, 1080p, 500 frames", [1.5, 1.7, 5.7, 6.3]),
        ("old_town_cross", "Film, 1080p, 500 frames", [1.2, 1.1, 7.0, 6.4]),
        ("park_joy", "Film, foliage, 1080p, 500 frames", [1.2, 0.9, 2.3, 2.3]),
        ("in_to_tree", "Film, native 4K, 100 frames", [0.8, 0.9, 27.4, 27.4]),
        ("aspen", "Camera, 4:2:2, 1080p, 300 frames", [0.9, 0.9, 4.4, 4.5]),
    ]
    names = ["regrain full", "regrain quarter", "grav1synth 0.3.8", "grav1synth 0.2.0"]
    chart(
        docs / "accuracy-live-action.svg",
        "Relative error on live action clips, lower is better. "
        + "; ".join(
            f"{clip}: " + ", ".join(f"{n} {v}%" for n, v in zip(names, values))
            for clip, _, values in clips
        ),
        [{
            "title": "Relative error on live action clips, lower is better",
            "legend": tools,
            "rows": [
                {
                    "label": esc(clip), "sub": material,
                    "bars": [(cls, v, f"{v}%") for (cls, _), v in zip(tools, values)],
                }
                for clip, material, values in clips
            ],
            "max": 28, "ticks": percent_ticks(25, 5), "right": 720,
        }],
    )

    known = [
        (("regrain",), "regrain", 0.014, 1.0),
        (("grav1synth 0.3.8",), "other", 0.064, 5.0),
        (("grav1synth 0.2.0",), "other", 0.067, 5.3),
    ]
    chart(
        docs / "accuracy-known.svg",
        "Relative error on known synthetic grain, lower is better. "
        + ", ".join(f"{' '.join(n)} {r}% ({e:.3f})" for n, _, e, r in known),
        [{
            "title": "Relative error on known grain, lower is better",
            "note": "Relative error in bold, then the error on the 0 to 255 scale",
            "rows": error_rows(known),
            "max": 6, "ticks": percent_ticks(6, 1), "right": 680,
        }],
    )

    speed = [
        (("grav1synth 0.2.0",), "other", 3),
        (("grav1synth 0.3.8",), "other", 58),
        (("regrain", "--scan full"), "regrain", 155),
        (("regrain", "--scan half"), "regrain", 230),
        (("regrain", "--scan quarter"), "regrain", 326),
        (("regrain", "--scan eighth"), "regrain", 405),
    ]
    chart(
        docs / "speed.svg",
        "Frames per second, higher is better. "
        + ", ".join(f"{' '.join(n)} {v}" for n, _, v in speed),
        [{
            "title": "Frames per second, higher is better",
            "rows": [{"label": label(*n), "bars": [(cls, v, str(v))]} for n, cls, v in speed],
            "max": 420, "ticks": [(t, str(t)) for t in range(0, 401, 100)], "right": 740,
        }],
    )

    chart(
        docs / "size.svg",
        "Size added to the 740 MB encode: regrain apply with default headers 0.21 MB, "
        "with --full-headers, the way grav1synth writes them, 3.88 MB. "
        "Film grain table entries: regrain 1124, grav1synth 0.3.8 2359. "
        "Film grain table size: regrain 414 KB, grav1synth 0.3.8 1.1 MB.",
        [
            {
                "title": "Added to the 740 MB encode, in MB",
                "rows": [
                    {"label": label("regrain apply"), "sub": "default headers",
                     "bars": [("regrain", 0.21, "0.21 MB")]},
                    {"label": label("regrain apply", "--full-headers"),
                     "sub": "the way grav1synth writes them",
                     "bars": [("other", 3.88, "3.88 MB")]},
                ],
                "max": 4, "ticks": [(t, str(t)) for t in range(5)], "right": 700,
            },
            {
                "title": "Entries in the film grain table",
                "rows": [
                    {"label": label("regrain"), "bars": [("regrain", 1124, "1124")]},
                    {"label": label("grav1synth 0.3.8"), "bars": [("other", 2359, "2359")]},
                ],
                "max": 2500, "ticks": [(t, str(t)) for t in range(0, 2501, 500)], "right": 700,
            },
            {
                "title": "Size of the film grain table, in KB",
                "rows": [
                    {"label": label("regrain"), "bars": [("regrain", 414, "414 KB")]},
                    {"label": label("grav1synth 0.3.8"), "bars": [("other", 1100, "1.1 MB")]},
                ],
                "max": 1200, "ticks": [(t, str(t)) for t in range(0, 1201, 200)], "right": 700,
            },
        ],
    )


if __name__ == "__main__":
    main()
