#!/usr/bin/env python3
"""Render the React-agent demo frames (react_frames.json) into SVG media.

Produces:
  docs/media/react-agent.svg            -- animated (CSS keyframes), 5 frames
  docs/media/react-agent-filmstrip.svg  -- static grid

Each frame shows the *actual* React output (parsed from the runtime's HTML),
the agent's DOM-query test results, and the model-call meter that makes the
replay-for-free story concrete. Pure stdlib.
"""
import json
import os
from html.parser import HTMLParser
from xml.sax.saxutils import escape

HERE = os.path.dirname(os.path.abspath(__file__))
FW, FH = 960, 480
PER = 3.2


# ----------------------------- HTML -> tree ----------------------------- #
class Node:
    def __init__(self, tag, attrs):
        self.tag = tag
        self.attrs = dict(attrs)
        self.children = []
        self.text = ""


class TB(HTMLParser):
    VOID = {"br", "img", "input", "hr", "meta", "link"}

    def __init__(self):
        super().__init__()
        self.root = Node("#root", [])
        self.stack = [self.root]

    def handle_starttag(self, tag, attrs):
        n = Node(tag, attrs)
        self.stack[-1].children.append(n)
        if tag not in self.VOID:
            self.stack.append(n)

    def handle_startendtag(self, tag, attrs):
        self.stack[-1].children.append(Node(tag, attrs))

    def handle_endtag(self, tag):
        if len(self.stack) > 1:
            self.stack.pop()

    def handle_data(self, data):
        if data.strip():
            self.stack[-1].text += data


def parse(html):
    tb = TB()
    tb.feed(html)
    return tb.root


def find(node, pred, out):
    for c in node.children:
        if pred(c):
            out.append(c)
        find(c, pred, out)


def cls(n):
    return n.attrs.get("class", "").split()


def card_model(html):
    root = parse(html)
    cards = []
    find(root, lambda n: n.tag == "div" and "card" in cls(n), out=cards)
    card = cards[0] if cards else root
    theme = "dark" if "dark" in cls(card) else "light"
    h2 = []
    find(card, lambda n: n.tag == "h2", h2)
    price = []
    find(card, lambda n: "price" in cls(n), price)
    lis = []
    find(card, lambda n: n.tag == "li", lis)
    btn = []
    find(card, lambda n: n.tag == "button", btn)
    return {
        "theme": theme,
        "title": h2[0].text if h2 else "",
        "price": price[0].text if price else "",
        "feats": [li.text for li in lis],
        "cta": btn[0].text if btn else "",
    }


# ----------------------------- SVG helpers ----------------------------- #
def rrect(x, y, w, h, r, fill, stroke=None, sw=1):
    s = f'<rect x="{x:.1f}" y="{y:.1f}" width="{w:.1f}" height="{h:.1f}" rx="{r}" fill="{fill}"'
    if stroke:
        s += f' stroke="{stroke}" stroke-width="{sw}"'
    return s + "/>"


def txt(x, y, s, size, fill, weight="400", anchor="start", mono=False):
    fam = ("ui-monospace,Menlo,Consolas,monospace" if mono
           else "system-ui,-apple-system,Segoe UI,Roboto,Helvetica,Arial,sans-serif")
    return (f'<text x="{x:.1f}" y="{y:.1f}" font-family="{fam}" font-size="{size}" '
            f'font-weight="{weight}" fill="{fill}" text-anchor="{anchor}">{escape(s)}</text>')


PAL = {
    "light": dict(bg="#ffffff", edge="#e2e8f0", text="#0f172a", sub="#64748b", accent="#4f46e5"),
    "dark": dict(bg="#0f172a", edge="#1e293b", text="#f1f5f9", sub="#94a3b8", accent="#38bdf8"),
}


def render_card(x, y, w, m, label=None):
    p = PAL[m["theme"]]
    out = []
    if label:
        out.append(txt(x + 6, y - 6, label[0], 11, p["accent"] if label[1] else "#94a3b8", "600"))
    # browser chrome + panel
    h = 92 + 28 * len(m["feats"]) + 56
    out.append(rrect(x, y, w, h, 12, p["bg"], p["edge"], 1.5))
    out.append(rrect(x, y, w, 26, 12, p["edge"]))
    out.append(rrect(x, y + 14, w, 12, 0, p["edge"]))
    for i, c in enumerate(["#ff5f57", "#febc2e", "#28c840"]):
        out.append(f'<circle cx="{x+16+i*14}" cy="{y+13}" r="4" fill="{c}"/>')
    bx = x + 22
    cy = y + 56
    out.append(txt(bx, cy, m["title"], 21, p["text"], "700"))
    cy += 8
    if m["price"]:
        cy += 30
        out.append(txt(bx, cy, m["price"], 26, p["accent"], "800"))
    cy += 18
    for f in m["feats"]:
        cy += 28
        out.append(f'<circle cx="{bx+4}" cy="{cy-4}" r="3" fill="{p["accent"]}"/>')
        out.append(txt(bx + 16, cy, f, 13.5, p["text"]))
    cy += 38
    out.append(rrect(bx, cy - 16, w - 44, 32, 8, p["accent"]))
    out.append(txt(x + w / 2, cy + 5, m["cta"], 13.5, "#ffffff", "700", "middle"))
    return "".join(out), h


def render_tests(x, y, w, tests, heading="acceptance tests"):
    out = [rrect(x, y, w, 44 + 30 * len(tests), 12, "#0b1020", "#1e293b", 1.5)]
    out.append(txt(x + 16, y + 26, heading, 12, "#64748b", "700", mono=True))
    ly = y + 50
    for t in tests:
        ok = t["pass"]
        col = "#22c55e" if ok else "#ef4444"
        out.append(f'<circle cx="{x+22}" cy="{ly-4}" r="8" fill="{col}"/>')
        out.append(txt(x + 22, ly, "✓" if ok else "✗", 11, "#0b1020", "900", "middle"))
        out.append(txt(x + 40, ly, t["name"], 12.5, "#cbd5e1" if ok else "#fca5a5"))
        ly += 30
    return "".join(out)


def meter(x, y, calls, replayed):
    out = [rrect(x, y, 250, 30, 15, "#eef2ff", "#c7d2fe", 1)]
    label = f"🧠 {calls} LLM calls"
    if replayed:
        label += f"  ·  {replayed} replayed free"
    out.append(txt(x + 16, y + 20, label, 12.5, "#4338ca", "700"))
    return "".join(out)


def frame_body(fr):
    out = [txt(28, 36, fr["caption"], 20, "#0f172a", "800")]
    out.append(txt(28, 56, fr["note"], 12.5, "#64748b"))
    out.append(meter(FW - 278, 20, fr["model_calls"], fr["replayed"]))
    top = 80
    if fr.get("html_b"):
        a, _ = render_card(28, top, 270, card_model(fr["html"]), ("original", False))
        b, _ = render_card(322, top, 270, card_model(fr["html_b"]), ("forked + edited", True))
        out.append(a)
        out.append(b)
        out.append(render_tests(620, top, FW - 620 - 24, fr["tests_b"] or fr["tests"],
                                "variant · all green"))
    else:
        c, _ = render_card(28, top, 360, card_model(fr["html"]))
        out.append(c)
        out.append(render_tests(420, top, FW - 420 - 24, fr["tests"]))
    return "".join(out)


def animated(frames):
    n = len(frames)
    total = n * PER
    css = ["text{dominant-baseline:alphabetic}"]
    groups = []
    eps = 0.01
    for i, fr in enumerate(frames):
        a = (i / n) * 100
        b = ((i + 1) / n) * 100
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
    cols = 2
    rows = (len(frames) + 1) // cols
    gap = 16
    scale = 0.6
    cw, ch = FW * scale, FH * scale
    W = cols * cw + (cols + 1) * gap
    H = rows * ch + (rows + 1) * gap
    cells = []
    for i, fr in enumerate(frames):
        r, c = divmod(i, cols)
        x = gap + c * (cw + gap)
        y = gap + r * (ch + gap)
        cells.append(f'<g transform="translate({x:.1f},{y:.1f}) scale({scale})">'
                     f'<rect width="{FW}" height="{FH}" rx="14" fill="#ffffff" stroke="#e2e8f0" stroke-width="2"/>'
                     f"{frame_body(fr)}</g>")
    return (f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W:.0f} {H:.0f}" width="{W:.0f}" height="{H:.0f}">'
            f'<rect width="{W:.0f}" height="{H:.0f}" fill="#eef2f6"/>{"".join(cells)}</svg>')


def main():
    frames = json.load(open(os.path.join(HERE, "react_frames.json")))
    open(os.path.join(HERE, "react-agent.svg"), "w").write(animated(frames))
    open(os.path.join(HERE, "react-agent-filmstrip.svg"), "w").write(filmstrip(frames))
    print("wrote react-agent.svg and react-agent-filmstrip.svg")


if __name__ == "__main__":
    main()
