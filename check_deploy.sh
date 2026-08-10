#!/bin/bash
cd /home/ubuntu/.myclaw/workspace/MyClaw
echo "=== git log ==="
git log --oneline -12
echo "=== ancestor check 355e68a -> 2106b96 ==="
git merge-base --is-ancestor 355e68a 2106b96 && echo "355e68a IS ancestor of 2106b96" || echo "355e68a NOT ancestor of 2106b96"
echo "=== ancestor check 2106b96 -> HEAD ==="
git merge-base --is-ancestor 2106b96 HEAD && echo "2106b96 IS ancestor of HEAD" || echo "2106b96 NOT ancestor of HEAD"
echo "=== process start ==="
ps -o pid,lstart,cmd -p 2729231 2>/dev/null
echo "=== systemd timestamps ==="
systemctl --user show myclaw -p ActiveEnterTimestamp -p ExecMainStartTimestamp 2>/dev/null | head -4
