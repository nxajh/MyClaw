#!/usr/bin/env python3
"""Module score — quantitative interface metrics for Rust modules (RFC
docs/agent-module-split-rfc.md §4.6 拆分质量闸).

Advisory only: always exits 0, never blocks. Intended to be run before and
after each split batch so the symbol counts land in the PR description.

Per .rs file it reports:
  - total_lines:            line count
  - pub_symbols:            count of `pub`/`pub(crate)`/`pub(super)` items of
                            kind fn/struct/enum/const/static/type/trait
  - private_fns:            count of non-pub `fn` items
  - struct_pub_fields:      count of `pub` fields across struct definitions
  - pub_use_reexports:      count of `pub use` re-export statements
  - pub_symbol_names:       sorted names of the pub symbols (JSON only)

Usage:
  python3 scripts/module_score.py src/agents/agent.rs
  python3 scripts/module_score.py --json src/agents/agent/ > out.json
  python3 scripts/module_score.py --json src/agents/ | python3 -m json.tool
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

PUB_ITEM = re.compile(
    r"^\s*(pub(?:\s*\([^)]*\))?\s+)"
    r"(?P<kind>fn|struct|enum|const|static|type|trait)\s+(?P<name>\w+)"
)
PRIVATE_FN = re.compile(r"^\s*fn\s+(?P<name>\w+)")
STRUCT_PUB_FIELD = re.compile(r"^\s*pub(?:\s*\([^)]*\))?\s+\w+\s*:")
PUB_USE = re.compile(r"^\s*pub(?:\s*\([^)]*\))?\s+use\s+")


def strip_comments_and_strings(text: str) -> str:
    """Remove // comments and string literals so regexes do not match inside
    them (doc-comment prose mentioning `pub fn` etc. would skew counts)."""
    out_lines = []
    for line in text.splitlines():
        # Walk the line, tracking whether we are inside a string literal.
        buf = []
        i = 0
        in_str = False
        while i < len(line):
            ch = line[i]
            if in_str:
                if ch == "\\":
                    i += 2  # skip escaped char
                    continue
                if ch == '"':
                    in_str = False
                i += 1
                continue
            if ch == '"':
                in_str = True
                i += 1
                continue
            if ch == "/" and i + 1 < len(line) and line[i + 1] == "/":
                break  # rest of line is a comment
            buf.append(ch)
            i += 1
        out_lines.append("".join(buf))
    return "\n".join(out_lines)


def score_file(path: Path) -> dict:
    text = path.read_text(encoding="utf-8", errors="replace")
    stripped = strip_comments_and_strings(text)
    pub_symbol_names: list[str] = []
    private_fns = 0
    struct_pub_fields = 0
    pub_use_reexports = 0
    in_struct_body = False
    brace_depth_at_struct = 0

    depth = 0
    for raw in stripped.splitlines():
        line = raw  # comments/strings already blanked
        m = PUB_ITEM.match(line)
        if m:
            pub_symbol_names.append(m.group("name"))
        elif PRIVATE_FN.match(line):
            private_fns += 1
        if PUB_USE.match(line):
            pub_use_reexports += 1

        # Track struct bodies to count pub fields (only top-level fields of a
        # struct literal, i.e. lines one brace deeper than the struct header).
        if in_struct_body and depth == brace_depth_at_struct + 1:
            if STRUCT_PUB_FIELD.match(line):
                struct_pub_fields += 1

        if re.search(r"\bstruct\s+\w+", line):
            in_struct_body = True
            brace_depth_at_struct = depth
        opens = line.count("{")
        closes = line.count("}")
        if in_struct_body and depth + opens - closes <= brace_depth_at_struct:
            in_struct_body = False
        depth += opens - closes

    return {
        "file": path.as_posix(),
        "total_lines": len(text.splitlines()),
        "pub_symbols": len(pub_symbol_names),
        "private_fns": private_fns,
        "struct_pub_fields": struct_pub_fields,
        "pub_use_reexports": pub_use_reexports,
        "pub_symbol_names": sorted(set(pub_symbol_names)),
    }


def gather_files(target: Path) -> list[Path]:
    if target.is_file():
        return [target]
    return sorted(p for p in target.rglob("*.rs"))


def main() -> int:
    ap = argparse.ArgumentParser(description="Rust module interface metrics")
    ap.add_argument("path", help=".rs file or directory to score")
    ap.add_argument("--json", action="store_true", help="emit JSON only")
    args = ap.parse_args()

    target = Path(args.path)
    if not target.exists():
        print(f"error: {target} not found", file=sys.stderr)
        return 0  # advisory: never fail hard

    files = gather_files(target)
    results = [score_file(p) for p in files]

    if args.json:
        print(json.dumps({"files": results}, ensure_ascii=False, indent=2))
        return 0

    header = (
        f"{'file':<44} {'lines':>6} {'pub':>4} {'privfn':>7} "
        f"{'pubfld':>7} {'reexp':>6}"
    )
    print(header)
    print("-" * len(header))
    for r in results:
        print(
            f"{r['file']:<44} {r['total_lines']:>6} {r['pub_symbols']:>4} "
            f"{r['private_fns']:>7} {r['struct_pub_fields']:>7} "
            f"{r['pub_use_reexports']:>6}"
        )
    print("-" * len(header))
    totals = {
        k: sum(r[k] for r in results)
        for k in ("total_lines", "pub_symbols", "private_fns",
                  "struct_pub_fields", "pub_use_reexports")
    }
    print(
        f"{'TOTAL (' + str(len(results)) + ' files)':<44} "
        f"{totals['total_lines']:>6} {totals['pub_symbols']:>4} "
        f"{totals['private_fns']:>7} {totals['struct_pub_fields']:>7} "
        f"{totals['pub_use_reexports']:>6}"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
