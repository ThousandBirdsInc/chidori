#!/usr/bin/env bash
#
# Mechanical guard for docs↔binary drift. docs/index.md tells readers to
# trust `chidori <command> --help` over any page; this check holds the pages
# to that bar so the drift is caught in CI instead of by an adopter:
#
#   1. every `CHIDORI_*` environment variable named anywhere in docs/ (or the
#      README / llm.txt) is actually read somewhere in the source tree;
#   2. every `chidori <subcommand>` named in docs/cli.md exists in the binary;
#   3. every `--flag` named in docs/cli.md exists on some (nested) subcommand.
#
# Usage: scripts/docs-drift-check.sh [path-to-chidori-binary]
# (default: target/debug/chidori — CI runs it right after `cargo test`, which
# leaves that binary behind.)

set -euo pipefail
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${1:-$REPO_ROOT/target/debug/chidori}"

if [[ ! -x "$BIN" ]]; then
  echo "error: $BIN not found or not executable (build with \`cargo build\` first)" >&2
  exit 2
fi

python3 - "$REPO_ROOT" "$BIN" << 'PY'
import re
import subprocess
import sys
from pathlib import Path

repo = Path(sys.argv[1])
binary = sys.argv[2]
problems: list[str] = []

# --- 1. env vars -----------------------------------------------------------
doc_sources = list((repo / "docs").rglob("*.md")) + [repo / "README.md", repo / "llm.txt"]
doc_vars: set[str] = set()
for f in doc_sources:
    if f.exists():
        doc_vars.update(re.findall(r"\bCHIDORI_[A-Z0-9_]+\b", f.read_text(errors="replace")))

grep = subprocess.run(
    ["grep", "-rhoE", "CHIDORI_[A-Z0-9_]+",
     str(repo / "crates"), str(repo / "sdk"), str(repo / "scripts"),
     str(repo / "integrations")],
    capture_output=True, text=True,
)
code_vars = set(grep.stdout.split())
for var in sorted(doc_vars - code_vars):
    # A name ending in `_` is a documented FAMILY prefix (e.g. CHIDORI_ISOLATE_*),
    # fine as long as at least one concrete member exists.
    if var.endswith("_") and any(v.startswith(var) for v in code_vars):
        continue
    problems.append(f"env var {var} is documented but never read by the code")

# --- helpers over --help ---------------------------------------------------
def help_text(*argv: str) -> str:
    out = subprocess.run([binary, *argv, "--help"], capture_output=True, text=True)
    return out.stdout + out.stderr

def commands_of(text: str) -> list[str]:
    section = text.split("Commands:", 1)
    if len(section) < 2:
        return []
    names = []
    for line in section[1].splitlines():
        m = re.match(r"^  ([a-z][a-z0-9-]*)\b", line)
        if m and m.group(1) != "help":
            names.append(m.group(1))
        elif line and not line.startswith(" "):
            break  # next help section
    return names

# Walk the (nested) subcommand tree, collecting every flag the binary accepts.
top = help_text()
subcommands = set(commands_of(top))
all_flags = set(re.findall(r"(--[a-z][a-z0-9-]*)", top))
frontier = [(name,) for name in subcommands]
seen = set(frontier)
while frontier:
    path = frontier.pop()
    text = help_text(*path)
    all_flags.update(re.findall(r"(--[a-z][a-z0-9-]*)", text))
    if len(path) < 3:
        for child in commands_of(text):
            nxt = path + (child,)
            if nxt not in seen:
                seen.add(nxt)
                frontier.append(nxt)

# --- 2 + 3. docs/cli.md ----------------------------------------------------
cli_doc = (repo / "docs" / "cli.md").read_text(errors="replace")
for sub in sorted(set(re.findall(r"`chidori ([a-z][a-z0-9-]*)", cli_doc))
                  | set(re.findall(r"^chidori ([a-z][a-z0-9-]*)", cli_doc, re.M))):
    if sub not in subcommands:
        problems.append(f"docs/cli.md names `chidori {sub}`, which the binary does not have")
for flag in sorted(set(re.findall(r"(--[a-z][a-z0-9-]*)", cli_doc))):
    if flag not in all_flags:
        problems.append(f"docs/cli.md names {flag}, which no (nested) subcommand accepts")

if problems:
    print("docs drift detected:")
    for p in problems:
        print(f"  - {p}")
    sys.exit(1)
print(f"docs drift check: OK ({len(doc_vars)} env vars, "
      f"{len(subcommands)} subcommands, {len(all_flags)} flags verified)")
PY
