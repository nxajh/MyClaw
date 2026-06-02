#!/bin/bash
# I58: Migrate workspace to mandatory `main` agent layout.
#
# RFC v2 §三.A requires every workspace to define a `main` agent at
# `workspace/agents/main/AGENT.md`. Before: bootstrap files (IDENTITY.md /
# SOUL.md / USER.md) lived at workspace root and shaped the default agent
# implicitly. After: agents/main/ holds an explicit AGENT.md whose body is
# the agent prose; bootstrap files are deleted.
#
# This script:
#   1. Creates workspace/agents/main/AGENT.md (skeleton) if missing.
#   2. Folds workspace/IDENTITY.md + SOUL.md content into AGENT.md body if found.
#   3. Backs up the originals to workspace/.migration_backup/ (does not delete).
#
# After running, the user must:
#   - Edit workspace/agents/main/AGENT.md to taste.
#   - Move any RULES.md content into AGENT.md.
#   - Delete the .migration_backup/ directory once happy.
#
# Usage: ./scripts/migrate_main_agent.sh [workspace_dir]

set -euo pipefail

WORKSPACE="${1:-$HOME/.myclaw/workspace}"
AGENTS_DIR="$WORKSPACE/agents"
MAIN_DIR="$AGENTS_DIR/main"
MAIN_FILE="$MAIN_DIR/AGENT.md"
BACKUP="$WORKSPACE/.migration_backup"

echo "=== MyClaw migrate_main_agent ==="
echo "Workspace: $WORKSPACE"
echo ""

if [ ! -d "$WORKSPACE" ]; then
  echo "ERROR: workspace not found: $WORKSPACE" >&2
  exit 1
fi

mkdir -p "$MAIN_DIR" "$BACKUP"

if [ -f "$MAIN_FILE" ]; then
  echo "✓ main agent already exists at $MAIN_FILE — skipping body merge"
  exit 0
fi

{
  echo "---"
  echo "name: main"
  echo "description: Main agent for this workspace."
  echo "---"
  echo ""
} > "$MAIN_FILE"

for src in IDENTITY.md SOUL.md; do
  src_path="$WORKSPACE/$src"
  if [ -f "$src_path" ]; then
    echo "## $src"  >> "$MAIN_FILE"
    echo ""        >> "$MAIN_FILE"
    cat "$src_path" >> "$MAIN_FILE"
    echo ""        >> "$MAIN_FILE"
    cp "$src_path" "$BACKUP/$src"
    echo "✓ folded $src → AGENT.md (backup at $BACKUP/$src)"
  fi
done

if [ ! -s "$MAIN_FILE" ] || [ "$(wc -l < "$MAIN_FILE")" -lt 5 ]; then
  echo ""
  echo "WARN: no IDENTITY.md / SOUL.md found. Wrote a minimal skeleton —"
  echo "      edit $MAIN_FILE before starting MyClaw."
fi

echo ""
echo "Migration complete. Inspect $MAIN_FILE and remove $BACKUP when satisfied."
