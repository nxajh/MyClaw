#!/bin/bash
# I59: Migrate workspace/memory/ to per-user layout.
#
# RFC v2 §三.D moves memory storage from a single shared
# `workspace/memory/*.md` flat directory to `workspace/users/{user_id}/memory/`.
# Each user's memories live under their own subtree so multiple users sharing
# a workspace don't leak context to each other.
#
# This script:
#   1. Looks for the shared workspace/memory/ directory.
#   2. Prompts for the user_id to assign existing memories to (default: "default").
#   3. Moves memory/*.md into workspace/users/{user_id}/memory/.
#   4. Leaves a marker file so subsequent reads from old location fail loudly.
#
# Run-as: the user owning the workspace data.
#
# Usage: ./scripts/migrate_memory.sh [workspace_dir] [user_id]

set -euo pipefail

WORKSPACE="${1:-$HOME/.myclaw/workspace}"
USER_ID="${2:-default}"
SRC="$WORKSPACE/memory"
DST="$WORKSPACE/users/$USER_ID/memory"

echo "=== MyClaw migrate_memory ==="
echo "Workspace: $WORKSPACE"
echo "User ID:   $USER_ID"
echo ""

if [ ! -d "$SRC" ]; then
  echo "Nothing to migrate — $SRC does not exist."
  exit 0
fi

shopt -s nullglob
mds=("$SRC"/*.md)
shopt -u nullglob

if [ ${#mds[@]} -eq 0 ]; then
  echo "Nothing to migrate — $SRC contains no .md files."
  exit 0
fi

mkdir -p "$DST"
moved=0
for f in "${mds[@]}"; do
  name=$(basename "$f")
  if [ -e "$DST/$name" ]; then
    echo "  skipping $name — already at destination"
    continue
  fi
  mv "$f" "$DST/$name"
  echo "  moved $name"
  moved=$((moved + 1))
done

# Marker so any code still reading the old path crashes loudly with intent.
echo "Migrated to $DST on $(date -u +%FT%TZ). Delete this file once verified." \
  > "$SRC/MIGRATED_TO_USERS.txt"

echo ""
echo "Moved $moved file(s). Inspect $DST and remove $SRC/MIGRATED_TO_USERS.txt"
echo "(and the now-empty $SRC) when satisfied."
