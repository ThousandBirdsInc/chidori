#!/usr/bin/env python3
"""Render the branch→feedback→converge demo (branch_frames.json) into SVG.

Produces:
  docs/media/branch-converge.svg            -- animated (CSS keyframes)
  docs/media/branch-converge-filmstrip.svg  -- static grid

Reuses the card/test renderers from render_react_demo. Pure stdlib.
"""
import json
import os
from render_react_demo import card_model, render_card, render_tests, txt, rrect

HERE = os.path.dirname(os.path.abspath(__file__))
FW, FH = 1000, 520
PER = 3.4


def meter(x, y, calls, inputs, replayed):
    out = [rrect(x, y, 300, 30, 15, "#eef2ff", "#c7d2fe", 1)]
    label = f"🧠 {calls} model · 👤 {inputs} input"
    if replayed:
        label += f" · {replayed} free"
    out.append(txt(x + 14, y + 20, label, 12, "#4338ca", "700"))
    return "".join(out)


def frame_body(fr):
    out = [txt(28, 36, fr["caption"], 20, "#0f172a", "800"),
           txt(28, 56, fr["note"], 12.5, "#64748b"),
           meter(FW - 320, 18, fr["model_calls"], fr["inputs"], fr["replayed"])]

    if fr["kind"] in ("branches", "feedback"):
        top = 96
        cw, gap, x0 = 300, 14, 24
        for i, c in enumerate(fr["cards"]):
            x = x0 + i * (cw + gap)
            svg, h = render_card(x, top, cw, card_model(c["html"]), (c["label"], c["chosen"]))
            if c["chosen"]:
                out.append(rrect(x - 7, top - 20, cw + 14, h + 54, 16, "none", "#6366f1", 3))
            out.append(svg)
            passed = sum(t["pass"] for t in c["tests"])
            summary = ("✓ user's pick · " if c["chosen"] else "") + f"{passed}/{len(c['tests'])} tests · {c['features']} feats"
            col = "#4338ca" if c["chosen"] else "#475569"
            out.append(txt(x + cw / 2, top + h + 24, summary, 12, col, "700", "middle"))
        if fr["kind"] == "feedback":
            by = FH - 64
            out.append(rrect(24, by, FW - 48, 50, 12, "#0b1020", "#1e293b", 1.5))
            out.append(txt(42, by + 21, "👤  " + fr["question"], 12.5, "#cbd5e1", "600"))
            out.append(txt(42, by + 40, "→  " + fr["answer"], 13.5, "#34d399", "800"))
    else:
        m = card_model(fr["html"])
        svg, h = render_card(80, 104, 390, m)
        out.append(svg)
        out.append(render_tests(520, 104, FW - 520 - 32, fr["tests"]))
        ny = 104 + h + 14
        green = fr["kind"] != "replay"
        out.append(rrect(80, ny, FW - 80 - 32, 48, 12,
                         "#ecfdf5" if green else "#eef2ff",
                         "#a7f3d0" if green else "#c7d2fe", 1.5))
        out.append(txt(98, ny + 30, fr["note"], 13, "#065f46" if green else "#4338ca", "700"))
    return "".join(out)


def animated(frames):
    n = len(frames)
    total = n * PER
    css = ["text{dominant-baseline:alphabetic}"]
    groups = []
    eps = 0.01
    for i, fr in enumerate(frames):
        a, b = (i / n) * 100, ((i + 1) / n) * 100
        stops = [(0.0, 1 if i == 0 else 0)]
        if a > 0:
            stops += [(a - eps, 0), (a, 1)]
        stops += [(b - eps, 1)]
        if b < 100:
            stops += [(b, 0)]
        stops += [(100.0, 1 if i == n - 1 else 0)]
        seen = {}
        for p, o in stops:
            seen[round(min(100.0, max(0.0, p)), 3)] = o
        body = "".join(f"{p:g}%{{opacity:{o}}}" for p, o in sorted(seen.items()))
        css.append(f"@keyframes f{i}{{{body}}}")
        css.append(f".g{i}{{opacity:0;animation:f{i} {total:.0f}s infinite}}")
        groups.append(f'<g class="g{i}"><rect width="{FW}" height="{FH}" fill="#f8fafc"/>{frame_body(fr)}</g>')
    return (f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {FW} {FH}" width="{FW}" height="{FH}">'
            f"<style>{''.join(css)}</style>"
            f'<rect width="{FW}" height="{FH}" rx="14" fill="#f8fafc"/>{"".join(groups)}</svg>')


def filmstrip(frames):
    cols = 1
    gap = 16
    scale = 0.66
    cw, ch = FW * scale, FH * scale
    W = cw + 2 * gap
    H = len(frames) * ch + (len(frames) + 1) * gap
    cells = []
    for i, fr in enumerate(frames):
        y = gap + i * (ch + gap)
        cells.append(f'<g transform="translate({gap},{y:.1f}) scale({scale})">'
                     f'<rect width="{FW}" height="{FH}" rx="14" fill="#ffffff" stroke="#e2e8f0" stroke-width="2"/>'
                     f"{frame_body(fr)}</g>")
    return (f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W:.0f} {H:.0f}" width="{W:.0f}" height="{H:.0f}">'
            f'<rect width="{W:.0f}" height="{H:.0f}" fill="#eef2f6"/>{"".join(cells)}</svg>')


def main():
    frames = json.load(open(os.path.join(HERE, "branch_frames.json")))
    open(os.path.join(HERE, "branch-converge.svg"), "w").write(animated(frames))
    open(os.path.join(HERE, "branch-converge-filmstrip.svg"), "w").write(filmstrip(frames))
    print("wrote branch-converge.svg and branch-converge-filmstrip.svg")


if __name__ == "__main__":
    main()
