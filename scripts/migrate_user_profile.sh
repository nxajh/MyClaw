#!/bin/bash
# I60: Migrate workspace/USER.md to per-user profile.toml.
#
# RFC v2 §三.D replaces the single workspace-level USER.md with per-user
# profile files at `workspace/users/{user_id}/profile.toml`. The TOML format
# carries structured fields (name, preferred_language, custom_instructions)
# that the prompt builder can inject into the system prompt.
#
# Conversion is best-effort: USER.md was free-form Markdown; we copy the
# whole content into `custom_instructions`. The user is expected to edit
# the resulting profile.toml to add structured fields.
#
# Usage: ./scripts/migrate_user_profile.sh [workspace_dir] [user_id]

set -euo pipefail

WORKSPACE="${1:-$HOME/.myclaw/workspace}"
USER_ID="${2:-default}"
SRC="$WORKSPACE/USER.md"
DST_DIR="$WORKSPACE/users/$USER_ID"
DST="$DST_DIR/profile.toml"

echo "=== MyClaw migrate_user_profile ==="
echo "Workspace: $WORKSPACE"
echo "User ID:   $USER_ID"
echo ""

if [ ! -f "$SRC" ]; then
  echo "Nothing to migrate — $SRC does not exist."
  exit 0
fi

mkdir -p "$DST_DIR"

if [ -f "$DST" ]; then
  echo "ERROR: $DST already exists. Refusing to overwrite." >&2
  echo "Move it aside and re-run." >&2
  exit 1
fi

# TOML triple-quoted string preserves newlines; escape \" → \\\".
content=$(sed 's/"""/\\"\\"\\"/g' "$SRC")

{
  echo "# Migrated from workspace/USER.md on $(date -u +%FT%TZ)."
  echo "# Edit to add structured fields (name, preferred_language, etc.)."
  echo ""
  echo 'custom_instructions = """'
  printf '%s\n' "$content"
  echo '"""'
} > "$DST"

# Stash the source.
mv "$SRC" "$WORKSPACE/.USER.md.migrated"

echo "✓ wrote $DST"
echo "✓ original archived at $WORKSPACE/.USER.md.migrated"
echo ""
echo "Inspect $DST and delete $WORKSPACE/.USER.md.migrated when satisfied."
