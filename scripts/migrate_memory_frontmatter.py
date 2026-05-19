#!/usr/bin/env python3
"""Migrate memory frontmatter: description → abstract + tags.

Usage:
    python3 scripts/migrate_memory_frontmatter.py [--dry-run] [--dir PATH]

Changes:
    - description → abstract (renamed)
    - Auto-generate tags from content keywords
    - Remove updated_at (not used by code)

Does NOT touch files without frontmatter.
"""

import os
import re
import sys
import argparse
from pathlib import Path


# ── Tag generation rules ──────────────────────────────────────────────────

KEYWORD_TAGS: list[tuple[list[str], str]] = [
    (["qqbot", "qq bot", "qq_bot"], "qqbot"),
    (["telegram"], "telegram"),
    (["heartbeat", "scheduler"], "scheduler"),
    (["skill"], "skill"),
    (["memory"], "memory"),
    (["provider"], "provider"),
    (["compaction", "truncate"], "compaction"),
    (["session", "recover"], "session"),
    (["mcp"], "mcp"),
    (["webui", "web ui", "caddy", "vite"], "webui"),
    (["iptables", "docker", "oracle cloud"], "infra"),
    (["token", "oauth", "cloak"], "auth"),
    (["cron"], "cron"),
    (["delegate", "sub-agent", "sub agent"], "delegate"),
    (["loop", "endless"], "loop-detection"),
    (["error", "400", "429", "bug"], "bug"),
    (["architecture", "multi-agent", "refactor"], "architecture"),
    (["claude", "anthropic"], "claude"),
    (["kimi", "k2.6"], "kimi"),
    (["minimax"], "minimax"),
    (["glm"], "glm"),
    (["search", "cooldown", "fallback"], "routing"),
    (["websocket", "channel"], "channel"),
    (["tui"], "tui"),
    (["reload", "restart", "sigusr"], "lifecycle"),
]

# Always include type as a tag
TYPE_TAGS = {
    "user": "user-preference",
    "feedback": "feedback",
    "project": "project",
    "reference": "reference",
}


def generate_tags(name: str, description: str, content: str, mem_type: str) -> list[str]:
    """Generate tags from content analysis."""
    text = f"{name} {description} {content[:2000]}".lower()
    tags = []

    # Add type tag
    if mem_type in TYPE_TAGS:
        tags.append(TYPE_TAGS[mem_type])

    # Keyword-based tags
    for keywords, tag in KEYWORD_TAGS:
        if any(kw in text for kw in keywords):
            if tag not in tags:
                tags.append(tag)

    return tags[:6]  # Cap at 6 tags


def migrate_file(filepath: Path, dry_run: bool = False) -> bool:
    """Migrate a single memory file. Returns True if changed."""
    raw = filepath.read_text(encoding="utf-8")
    stripped = raw.strip()

    if not stripped.startswith("---"):
        print(f"  SKIP (no frontmatter): {filepath.name}")
        return False

    # Find closing ---
    rest = stripped[3:]
    rest = rest.lstrip("\r\n")
    end = rest.find("\n---")
    if end == -1:
        print(f"  SKIP (malformed frontmatter): {filepath.name}")
        return False

    frontmatter = rest[:end]
    body = rest[end + 4:].strip()

    # Parse existing frontmatter
    fields = {}
    for line in frontmatter.splitlines():
        line = line.strip()
        if ":" in line:
            key, value = line.split(":", 1)
            fields[key.strip()] = value.strip()

    name = fields.get("name", filepath.stem)
    mem_type = fields.get("type", "project")
    description = fields.get("description", "")
    created_at = fields.get("created_at", "")
    # Strip quotes from description
    if description.startswith('"') and description.endswith('"'):
        description = description[1:-1]

    if not description:
        print(f"  SKIP (no description to migrate): {filepath.name}")
        return False

    # Generate tags from content
    tags = generate_tags(name, description, body[:2000], mem_type)

    # Build new frontmatter
    lines = ["---"]
    lines.append(f"name: {name}")
    lines.append(f'abstract: "{description}"')
    if tags:
        lines.append(f"tags: [{', '.join(tags)}]")
    lines.append(f"type: {mem_type}")
    lines.append(f"created_at: {created_at}")
    lines.append("---")

    new_content = "\n".join(lines) + "\n\n" + body + "\n"

    if dry_run:
        print(f"  WOULD MIGRATE: {filepath.name}")
        print(f"    abstract: {description[:60]}...")
        print(f"    tags: {tags}")
        return True

    filepath.write_text(new_content, encoding="utf-8")
    print(f"  MIGRATED: {filepath.name}")
    print(f"    abstract: {description[:60]}...")
    print(f"    tags: {tags}")
    return True


def main():
    parser = argparse.ArgumentParser(description="Migrate memory frontmatter")
    parser.add_argument("--dry-run", action="store_true", help="Preview changes without writing")
    parser.add_argument("--dir", default=None, help="Memory directory path")
    args = parser.parse_args()

    memory_dir = Path(args.dir) if args.dir else Path(__file__).parent.parent / "workspace" / "memory"

    if not memory_dir.exists():
        print(f"Error: directory not found: {memory_dir}")
        sys.exit(1)

    md_files = sorted(memory_dir.glob("*.md"))
    print(f"Scanning {len(md_files)} files in {memory_dir}")
    if args.dry_run:
        print("(DRY RUN - no changes will be written)")
    print()

    migrated = 0
    for f in md_files:
        if migrate_file(f, dry_run=args.dry_run):
            migrated += 1

    print(f"\n{'Would migrate' if args.dry_run else 'Migrated'}: {migrated}/{len(md_files)} files")


if __name__ == "__main__":
    main()
