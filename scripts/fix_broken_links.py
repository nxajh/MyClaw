#!/usr/bin/env python3
"""Fix broken See Also links by replacing non-existent targets with valid ones."""
import re, sys
from pathlib import Path

def main():
    memory_dir = Path("/home/ubuntu/.myclaw/workspace/memory")
    all_files = {f.stem for f in memory_dir.glob("*.md")}
    
    # Known name corrections
    corrections = {
        "server_infrastructure": "server_infra",
    }
    
    fixed = 0
    for path in sorted(memory_dir.glob("*.md")):
        raw = path.read_text(encoding="utf-8")
        new_raw = raw
        
        for wrong, right in corrections.items():
            new_raw = new_raw.replace(f"]({wrong}.md)", f"]({right}.md)")
            new_raw = new_raw.replace(f"]({wrong})", f"]({right}.md)")
        
        if new_raw != raw:
            path.write_text(new_raw, encoding="utf-8")
            fixed += 1
            print(f"FIXED: {path.name}")
    
    print(f"Fixed {fixed} files.")

if __name__ == "__main__":
    main()
