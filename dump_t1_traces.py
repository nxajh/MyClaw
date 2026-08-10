#!/usr/bin/env python3
import json, re

path = '/home/ubuntu/.myclaw/workspace/sessions/myclaw_s_019fe564-1566-7453-b9b0-89c5d707fa93/archive/history.0149.jsonl'
lines = open(path).readlines()

# 1) full line 93 text
obj = json.loads(lines[92])
txt = obj['parts'][0].get('text', '')
print('##### line 93 full text (len=%d) #####' % len(txt))
print(txt)
print()

# 2) occurrences of t1 / t2 task ids and markers
t1 = '019feb88-94da'
t2 = '019feb88-94df'
markers = ['[系统通知]', 'CONTEXT COMPACTION', '输出将作为进度通知', 'progress']
for i, line in enumerate(lines, 1):
    if t1 in line or t2 in line:
        # find which marker
        for m in markers:
            if m in line:
                print(f'line {i}: contains {t1 if t1 in line else t2} + {m!r}')
                break
        else:
            print(f'line {i}: contains {t1 if t1 in line else t2} (no marker)')
