#!/usr/bin/env python3
"""Render a Test262 conformance-coverage table (Markdown) from a runner state file.

The runner's `--state <file>` persists per-test results as
`{"results": {"test/<path>.js": "pass"|"fail"|"skip", ...}, "summary": {...}}`.
This script aggregates those by area and prints a Markdown report to stdout — an
overall pass-rate line, a compact top-level table, and a collapsed
per-subdirectory breakdown of the areas that still have failures.

Pass-rate is over *executed* tests (pass + fail); skipped tests (unsupported or
intentionally-out-of-scope features) are excluded from the percentage, matching
how the runner reports "% of executed".

Usage: test262-coverage.py <state.json>   (writes Markdown to stdout)
"""
import json
import sys

MARKER = "<!-- test262-coverage -->"


def pct(p, f):
    ran = p + f
    return (p * 100.0 / ran) if ran else 100.0


def add(table, key, status):
    p, f, s = table.get(key, (0, 0, 0))
    if status == "pass":
        p += 1
    elif status == "fail":
        f += 1
    else:
        s += 1
    table[key] = (p, f, s)


def area_keys(rel):
    """(top-level, second-level) area names for a `test/<...>.js` path."""
    parts = rel.split("/")
    # parts[0] == "test"; group at parts[1] (top) and parts[1]/parts[2] (sub).
    if len(parts) < 2:
        return ("(other)", "(other)")
    top = parts[1]
    sub = f"{parts[1]}/{parts[2]}" if len(parts) >= 3 else parts[1]
    return (top, sub)


def render_rows(table):
    rows = []
    for area, (p, f, s) in sorted(table.items(), key=lambda kv: (-kv[1][1], kv[0])):
        rows.append(f"| `{area}` | {p} | {f} | {s} | {pct(p, f):.2f}% |")
    return rows


def main():
    if len(sys.argv) != 2:
        sys.exit("usage: test262-coverage.py <state.json>")
    data = json.load(open(sys.argv[1]))
    results = data.get("results", data)

    tops, subs = {}, {}
    tp = tf = ts = 0
    for rel, status in results.items():
        top, sub = area_keys(rel)
        add(tops, top, status)
        add(subs, sub, status)
        if status == "pass":
            tp += 1
        elif status == "fail":
            tf += 1
        else:
            ts += 1

    total = tp + tf + ts
    out = []
    out.append(MARKER)
    out.append("## Test262 conformance coverage")
    out.append("")
    out.append(
        f"**{tp} / {tp + tf} executed pass ({pct(tp, tf):.2f}%)** · "
        f"{tf} fail · {ts} skip · {total} total"
    )
    out.append("")
    out.append("| Area | Pass | Fail | Skip | Pass-rate |")
    out.append("|---|--:|--:|--:|--:|")
    out.extend(render_rows(tops))
    out.append(f"| **Total** | **{tp}** | **{tf}** | **{ts}** | **{pct(tp, tf):.2f}%** |")
    out.append("")

    failing_subs = {k: v for k, v in subs.items() if v[1] > 0}
    if failing_subs:
        out.append("<details><summary>Per-subdirectory breakdown "
                   f"({len(failing_subs)} areas with failures)</summary>")
        out.append("")
        out.append("| Area | Pass | Fail | Skip | Pass-rate |")
        out.append("|---|--:|--:|--:|--:|")
        out.extend(render_rows(failing_subs))
        out.append("")
        out.append("</details>")
    out.append("")
    print("\n".join(out))


if __name__ == "__main__":
    main()
