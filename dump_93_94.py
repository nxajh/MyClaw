#!/usr/bin/env python3
import json, sys

path = '/home/ubuntu/.myclaw/workspace/sessions/myclaw_s_019fe564-1566-7453-b9b0-89c5d707fa93/archive/history.0149.jsonl'
with open(path) as f:
    lines = f.readlines()
print('total lines:', len(lines))

for ln in range(88, 98):
    obj = json.loads(lines[ln - 1])
    role = obj.get('role')
    print(f'===== line {ln} role={role} keys={list(obj.keys())}')
    for p in obj.get('parts', []):
        t = p.get('type')
        if t == 'thinking':
            val = p.get('thinking', '')
        else:
            val = p.get('text', '')
        print(f'  --- [{t}] len={len(val)}')
        print(val[:2000])
    print()
