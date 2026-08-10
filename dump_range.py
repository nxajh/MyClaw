#!/usr/bin/env python3
"""Dump role + truncated parts for a line range of a history jsonl."""
import json, sys

path = sys.argv[1]
start = int(sys.argv[2])
end = int(sys.argv[3])
maxlen = int(sys.argv[4]) if len(sys.argv) > 4 else 300

with open(path) as f:
    for idx, line in enumerate(f, 1):
        if idx < start or idx > end:
            continue
        line = line.strip()
        if not line:
            continue
        try:
            rec = json.loads(line)
        except Exception as e:
            print(f"{idx}: PARSE ERR {e}")
            continue
        role = rec.get("role", "?")
        parts = rec.get("parts", [])
        texts = []
        for p in parts:
            t = p.get("text") or p.get("thinking") or ""
            t = t.replace("\n", "\\n")
            texts.append(f"{p.get('type','?')}:{t[:maxlen]}")
        extra = ""
        if "tool_calls" in rec:
            extra = f" | tool_calls={len(rec['tool_calls'])}"
        if "model" in rec:
            extra += f" | model={rec.get('model')}"
        if "usage" in rec:
            u = rec.get("usage", {})
            extra += f" | stop={u.get('stop_reason')}"
        if "meta" in rec:
            extra += f" | meta={str(rec.get('meta'))[:200]}"
        print(f"--- {idx} role={role}{extra}")
        for t in texts:
            print(f"    {t}")
