#!/bin/bash
# Post-build fix for MyClaw WebUI deployed under /myclaw-ui/
# Run after: npm run build
DIR="$(cd "$(dirname "$0")" && pwd)/dist"
FILE="$DIR/index.html"

if [ ! -f "$FILE" ]; then
  echo "Error: $FILE not found" >&2
  exit 1
fi

# Replace Go template base href with hardcoded path
sed -i "s|{{if .Vars.base_path}}<base href=\"{{.Vars.base_path}}\" />{{else}}<base href=\"/\" />{{end}}|<base href=\"/myclaw-ui/\" />|" "$FILE"

# Replace relative asset paths with absolute prefixed paths
sed -i 's|src="./assets/|src="/myclaw-ui/assets/|g' "$FILE"
sed -i 's|href="./assets/|href="/myclaw-ui/assets/|g' "$FILE"

echo "Fixed: $FILE"
grep -E 'base href|index-' "$FILE"
