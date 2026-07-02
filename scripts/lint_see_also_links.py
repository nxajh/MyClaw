#!/usr/bin/env python3
"""Lint See Also links: verify every link target exists as a memory file."""
import os, re, sys
from pathlib import Path

def main():
    memory_dir = Path("/home/ubuntu/.myclaw/workspace/memory")
    all_files = {f.stem for f in memory_dir.glob("*.md")}
    
    errors = []
    checked = 0
    
    for path in sorted(memory_dir.glob("*.md")):
        raw = path.read_text(encoding="utf-8")
        lines = raw.split("\n")
        in_see_also = False
        
        for line in lines:
            trimmed = line.strip()
            if trimmed.startswith("## "):
                in_see_also = trimmed.lower() == "## see also"
                continue
            if not in_see_also:
                continue
            # Find markdown links
            for match in re.finditer(r'\[([^\]]*)\]\(([^)]+)\)', trimmed):
                target_raw = match.group(2)
                target = target_raw.rsplit('/', 1)[-1].replace('.md', '')
                checked += 1
                if target not in all_files:
                    errors.append(f"BROKEN: {path.name} -> {target_raw} (target '{target}' not found)")
    
    print(f"Checked {checked} links across {len(all_files)} files.")
    if errors:
        print(f"FAILED: {len(errors)} broken links found:")
        for e in errors:
            print(f"  {e}")
        sys.exit(1)
    else:
        print("ALL LINKS VALID ✓")

if __name__ == "__main__":
    main()
