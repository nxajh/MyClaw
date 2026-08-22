#!/usr/bin/env python3
"""Migrate SKILL.md frontmatter: converge non-standard top-level fields into `metadata:`.

Usage:
    python3 scripts/migrate_skill_frontmatter.py [--dry-run] [--dir PATH]

Per the Agent Skills standard (agentskills.io), `name`/`description`/`metadata`
are the only standard top-level frontmatter keys. This script moves every
other top-level key (keywords, version, when_to_use, argument_hint,
arguments, user_invocable, agent_invocable, status, homepage, or any other
non-standard key a skill happens to carry) under `metadata:`, merging with
whatever the file's `metadata:` block already has. `name` and `description`
are left untouched at the top level.

Known plain-string fields (version, when_to_use, argument_hint, status,
homepage) get quoted as strings if not already quoted, per the standard's
"metadata is a string-to-string map" requirement. Lists and booleans are
left as-is — the loader's YAML reader accepts them quoted or not.

Idempotent: a file with nothing left to move (already migrated, or never
had any non-standard top-level field) is left untouched — running this
script twice produces no further changes on the second pass.

Does NOT touch files without frontmatter, or malformed frontmatter (no
closing `---`).

See issue #125.
"""

import argparse
import re
import sys
from pathlib import Path

STANDARD_TOP_LEVEL_KEYS = {"name", "description", "metadata"}

# Known scalar (plain string) fields that should be quoted under metadata,
# per the standard's string-to-string map requirement for metadata values.
QUOTE_AS_STRING_KEYS = {"version", "when_to_use", "argument_hint", "status", "homepage"}

_KEY_LINE_RE = re.compile(r"^([A-Za-z_][A-Za-z0-9_-]*):(.*)$")


def parse_fields(text: str) -> list[tuple[str, str, list[str]]]:
    """Parse a block of `key: value` lines (with possible multi-line/nested
    continuations) into an ordered list of (key, header_rest, continuation_lines).

    A continuation is any run of lines following a key line that are either
    blank or start with whitespace — this captures multi-line YAML lists
    and nested mappings without needing a full YAML parser.
    """
    lines = text.split("\n")
    fields: list[tuple[str, str, list[str]]] = []
    i = 0
    while i < len(lines):
        line = lines[i]
        if line.strip() == "":
            i += 1
            continue
        if line[:1].isspace():
            # Orphan indented line with no preceding key at this level.
            i += 1
            continue
        m = _KEY_LINE_RE.match(line)
        if not m:
            i += 1
            continue
        key, rest = m.group(1), m.group(2)
        i += 1
        cont: list[str] = []
        while i < len(lines) and (lines[i] == "" or lines[i][:1].isspace()):
            cont.append(lines[i])
            i += 1
        fields.append((key, rest, cont))
    return fields


def dedent_block(cont_lines: list[str]) -> list[str]:
    """Strip the common leading-space indent from a continuation block."""
    non_empty = [l for l in cont_lines if l.strip() != ""]
    if not non_empty:
        return []
    indent = min(len(l) - len(l.lstrip(" ")) for l in non_empty)
    return [l[indent:] if len(l) >= indent else l for l in cont_lines]


def indent_block(cont_lines: list[str], by: int) -> list[str]:
    pad = " " * by
    return [pad + l if l.strip() != "" else l for l in cont_lines]


def maybe_quote_scalar(key: str, rest: str) -> str:
    """Quote a plain (unquoted) scalar value for known string fields.
    Leaves lists (`[...]`), block openers (empty rest), and already-quoted
    values untouched."""
    if key not in QUOTE_AS_STRING_KEYS:
        return rest
    value = rest.strip()
    if not value or value.startswith("[") or value.startswith('"') or value.startswith("'"):
        return rest
    escaped = value.replace('"', '\\"')
    return f' "{escaped}"'


def migrate_frontmatter(front_matter: str) -> tuple[str, bool]:
    """Return (new_front_matter, changed)."""
    fields = parse_fields(front_matter)

    name_field = None
    description_field = None
    metadata_subfields: list[tuple[str, str, list[str]]] = []
    to_move: list[tuple[str, str, list[str]]] = []

    for key, rest, cont in fields:
        if key == "name":
            name_field = (key, rest, cont)
        elif key == "description":
            description_field = (key, rest, cont)
        elif key == "metadata":
            metadata_subfields = parse_fields("\n".join(dedent_block(cont)))
        elif key not in STANDARD_TOP_LEVEL_KEYS:
            to_move.append((key, rest, cont))

    if not to_move:
        return front_matter, False

    # Merge: a top-level occurrence overrides an existing metadata subfield
    # of the same name (matching the loader's own dual-read priority).
    merged = list(metadata_subfields)
    merged_keys = {k for k, _, _ in merged}
    for key, rest, cont in to_move:
        rest = maybe_quote_scalar(key, rest)
        if key in merged_keys:
            merged = [(k, rest, cont) if k == key else (k, r, c) for k, r, c in merged]
        else:
            merged.append((key, rest, cont))
            merged_keys.add(key)

    out_lines: list[str] = []
    if name_field:
        key, rest, cont = name_field
        out_lines.append(f"{key}:{rest}")
        out_lines.extend(cont)
    if description_field:
        key, rest, cont = description_field
        out_lines.append(f"{key}:{rest}")
        out_lines.extend(cont)

    out_lines.append("metadata:")
    for key, rest, cont in merged:
        out_lines.append(f"  {key}:{rest}")
        out_lines.extend(indent_block(cont, 2))

    return "\n".join(out_lines), True


def migrate_file(filepath: Path, dry_run: bool = False) -> bool:
    """Migrate a single SKILL.md file. Returns True if changed (or would change)."""
    raw = filepath.read_text(encoding="utf-8")
    stripped = raw.strip()

    if not stripped.startswith("---"):
        print(f"  SKIP (no frontmatter): {filepath}")
        return False

    rest = stripped[3:].lstrip("\r\n")
    end = rest.find("\n---")
    if end == -1:
        print(f"  SKIP (malformed frontmatter): {filepath}")
        return False

    front_matter = rest[:end].strip("\n")
    body = rest[end + 4:]

    new_front_matter, changed = migrate_frontmatter(front_matter)
    if not changed:
        print(f"  SKIP (already migrated / nothing to move): {filepath}")
        return False

    new_content = "---\n" + new_front_matter + "\n---" + body
    if not new_content.endswith("\n"):
        new_content += "\n"

    if dry_run:
        print(f"  WOULD MIGRATE: {filepath}")
        return True

    filepath.write_text(new_content, encoding="utf-8")
    print(f"  MIGRATED: {filepath}")
    return True


def main() -> None:
    parser = argparse.ArgumentParser(description="Migrate SKILL.md frontmatter to metadata")
    parser.add_argument("--dry-run", action="store_true", help="Preview changes without writing")
    parser.add_argument("--dir", default=None, help="Skills directory (default: workspace/skills)")
    args = parser.parse_args()

    skills_dir = Path(args.dir) if args.dir else Path(__file__).parent.parent / "workspace" / "skills"

    if not skills_dir.exists():
        print(f"Error: directory not found: {skills_dir}")
        sys.exit(1)

    skill_files = sorted(skills_dir.glob("*/SKILL.md"))
    print(f"Scanning {len(skill_files)} SKILL.md files in {skills_dir}")
    if args.dry_run:
        print("(DRY RUN - no changes will be written)")
    print()

    migrated = 0
    for f in skill_files:
        if migrate_file(f, dry_run=args.dry_run):
            migrated += 1

    print(f"\n{'Would migrate' if args.dry_run else 'Migrated'}: {migrated}/{len(skill_files)} files")


if __name__ == "__main__":
    main()
