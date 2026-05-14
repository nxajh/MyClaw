#!/bin/bash
# Migrate session keys from 2-segment to 3-segment format.
#
# Old format: "channel_type:sender"
# New format: "channel_type:account_id:sender"
#
# Usage: ./scripts/migrate_session_keys.sh [sessions_dir] [last_channel_file]

set -euo pipefail

SESSIONS_DIR="${1:-$HOME/.myclaw/workspace/sessions}"
LC_FILE="${2:-$HOME/.myclaw/workspace/.last_channel}"

echo "=== MyClaw Session Key Migration ==="
echo "Sessions dir: $SESSIONS_DIR"
echo "Last channel file: $LC_FILE"
echo ""

# Migrate session files
if [ -d "$SESSIONS_DIR" ]; then
    count=0
    for f in "$SESSIONS_DIR"/*; do
        [ -f "$f" ] || continue
        filename=$(basename "$f")
        # Match 2-segment keys (e.g., "telegram:12345") but not 3-segment (e.g., "telegram:default:12345")
        if [[ "$filename" =~ ^([a-z]+):([^:]+)$ ]]; then
            channel="${BASH_REMATCH[1]}"
            sender="${BASH_REMATCH[2]}"
            new_name="${channel}:default:${sender}"
            echo "  migrating: $filename → $new_name"
            mv "$f" "$SESSIONS_DIR/$new_name"
            ((count++))
        fi
    done
    echo "Migrated $count session files."
else
    echo "Sessions directory not found: $SESSIONS_DIR"
fi

echo ""

# Migrate .last_channel file
if [ -f "$LC_FILE" ]; then
    content=$(cat "$LC_FILE" | tr -d '[:space:]')
    # Match single-word channel name (e.g., "telegram") but not "channel:account"
    if [[ "$content" =~ ^[a-z]+$ ]]; then
        echo "  migrating .last_channel: $content → ${content}:default"
        echo "${content}:default" > "$LC_FILE"
        echo "Migrated .last_channel."
    else
        echo ".last_channel already in new format or empty: '$content'"
    fi
else
    echo ".last_channel file not found: $LC_FILE"
fi

echo ""
echo "Migration complete."
