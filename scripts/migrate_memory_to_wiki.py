#!/usr/bin/env python3
"""
Normalize memory files to the Wiki format.

Transformations:
  1. summary/abstract → description (priority: description > summary > abstract)
  2. Add updated_at field if missing (set to today's date)
  3. Strip any old enum-only type constraints (type stays as-is, it's already a string)

Usage:
  python3 scripts/migrate_memory_to_wiki.py [--dry-run] [--memory-dir PATH]
"""

import os
import re
import sys
from datetime import date
from pathlib import Path


def parse_frontmatter(raw: str):
    """Split file into (frontmatter_lines, body). Returns (None, None) if no frontmatter."""
    trimmed = raw.strip()
    if not trimmed.startswith("---"):
        return None, None

    rest = trimmed[3:].lstrip("\r\n")
    end = rest.find("\n---")
    if end == -1:
        return None, None

    fm_text = rest[:end]
    body = rest[end + 4:].strip()
    return fm_text, body


def migrate_file(path: Path, today: str, dry_run: bool) -> str:
    """Migrate a single file. Returns status string."""
    raw = path.read_text(encoding="utf-8")
    fm_text, body = parse_frontmatter(raw)

    if fm_text is None:
        return f"SKIP (no frontmatter): {path.name}"

    fm_lines = fm_text.split("\n")
    new_lines = []
    changes = []

    has_description = False
    has_summary = False
    has_abstract = False
    has_updated_at = False
    summary_val = None
    abstract_val = None
    summary_line_idx = None
    abstract_line_idx = None

    # First pass: detect what we have
    for i, line in enumerate(fm_lines):
        key = line.strip().split(":")[0].strip() if ":" in line else ""
        if key == "description":
            has_description = True
        elif key == "summary":
            has_summary = True
            summary_val = line.strip().split(":", 1)[1].strip()
            summary_line_idx = i
        elif key == "abstract":
            has_abstract = True
            abstract_val = line.strip().split(":", 1)[1].strip()
            abstract_line_idx = i
        elif key == "updated_at":
            has_updated_at = True

    # Determine the description value
    # Priority: existing description > summary > abstract
    desc_val = None
    if has_description:
        desc_val = None  # keep existing
    elif has_summary:
        desc_val = summary_val
    elif has_abstract:
        desc_val = abstract_val

    # Second pass: build new frontmatter
    for i, line in enumerate(fm_lines):
        stripped = line.strip()
        if ":" not in stripped:
            new_lines.append(line)
            continue

        key = stripped.split(":")[0].strip()
        value = stripped.split(":", 1)[1].strip()

        if key == "summary":
            if not has_description:
                # Rename to description
                new_lines.append(line.replace("summary:", "description:", 1))
                changes.append("summary→description")
            else:
                # Description already exists, drop summary
                changes.append("dropped redundant summary")
            continue

        if key == "abstract":
            if not has_description and not has_summary:
                # Rename to description
                new_lines.append(line.replace("abstract:", "description:", 1))
                changes.append("abstract→description")
            else:
                # Description or summary already handled it, drop abstract
                changes.append("dropped redundant abstract")
            continue

        new_lines.append(line)

    # Add updated_at if missing (insert before closing, after created_at or at end)
    if not has_updated_at:
        # Find position to insert (after created_at or at end of frontmatter)
        insert_idx = len(new_lines)
        for i, line in enumerate(new_lines):
            if line.strip().startswith("created_at:"):
                insert_idx = i + 1
        new_lines.insert(insert_idx, f"updated_at: {today}")
        changes.append("added updated_at")

    # Rebuild file
    new_fm = "\n".join(new_lines)
    new_content = f"---\n{new_fm}\n---\n\n{body}\n"

    if new_content != raw:
        if not dry_run:
            path.write_text(new_content, encoding="utf-8")
        change_str = ", ".join(changes) if changes else "formatting"
        return f"{'DRY-RUN ' if dry_run else ''}UPDATED ({change_str}): {path.name}"
    else:
        return f"OK (no changes): {path.name}"


def main():
    dry_run = "--dry-run" in sys.argv
    memory_dir = Path(os.environ.get("MEMORY_DIR", "/home/ubuntu/.myclaw/workspace/memory"))

    # Allow override via --memory-dir
    for i, arg in enumerate(sys.argv):
        if arg == "--memory-dir" and i + 1 < len(sys.argv):
            memory_dir = Path(sys.argv[i + 1])

    if not memory_dir.exists():
        print(f"Error: memory directory not found: {memory_dir}")
        sys.exit(1)

    today = date.today().isoformat()
    md_files = sorted(memory_dir.glob("*.md"))

    print(f"{'DRY RUN: ' if dry_run else ''}Migrating {len(md_files)} files in {memory_dir}")
    print(f"Date for updated_at: {today}")
    print("=" * 70)

    updated = 0
    skipped = 0
    ok = 0

    for path in md_files:
        result = migrate_file(path, today, dry_run)
        print(result)
        if "UPDATED" in result:
            updated += 1
        elif "SKIP" in result:
            skipped += 1
        else:
            ok += 1

    print("=" * 70)
    print(f"Total: {len(md_files)} | Updated: {updated} | OK: {ok} | Skipped: {skipped}")


if __name__ == "__main__":
    main()
