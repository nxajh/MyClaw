#!/usr/bin/env python3
"""
Generate See Also links for memory files based on content similarity.

Strategy:
  1. Extract keywords from each file's name, tags, and content.
  2. Compute TF-IDF-like overlap scores between all pairs.
  3. For each file, suggest top-N related files as See Also links.
  4. Append/update the ## See Also section in each file.

Usage:
  python3 scripts/generate_see_also_links.py [--dry-run] [--memory-dir PATH] [--top-n 3]
"""

import os
import re
import sys
import math
from collections import Counter
from pathlib import Path
from datetime import date


def parse_file(path: Path):
    """Parse a .md file into (frontmatter_dict, body_text, raw)."""
    raw = path.read_text(encoding="utf-8")
    trimmed = raw.strip()
    if not trimmed.startswith("---"):
        return {}, raw, raw

    rest = trimmed[3:].lstrip("\r\n")
    end = rest.find("\n---")
    if end == -1:
        return {}, raw, raw

    fm_text = rest[:end]
    body = rest[end + 4:].strip()

    fm = {}
    for line in fm_text.split("\n"):
        if ":" in line:
            key, val = line.split(":", 1)
            fm[key.strip()] = val.strip().strip('"')

    return fm, body, raw


def tokenize(text: str) -> set:
    """Extract lowercase word tokens (2+ chars)."""
    return {w for w in re.findall(r'[a-z0-9_]+', text.lower()) if len(w) >= 2}


def extract_keywords(fm: dict, name: str, body: str) -> set:
    """Extract keywords from frontmatter + body."""
    keywords = set()
    keywords |= tokenize(name)
    keywords |= tokenize(fm.get("description", "") + " " + fm.get("summary", "") + " " + fm.get("abstract", ""))
    
    for tag in fm.get("tags", "").strip("[]").split(","):
        tag = tag.strip()
        if tag:
            keywords |= tokenize(tag)
    
    # Use first 500 chars of body for keyword extraction
    keywords |= tokenize(body[:500])
    return keywords


# Common stop words to reduce noise
STOP_WORDS = {
    "the", "and", "for", "that", "with", "this", "from", "are", "was", "were",
    "been", "have", "has", "had", "not", "but", "all", "can", "her", "was",
    "one", "our", "out", "day", "get", "has", "him", "his", "how", "its",
    "may", "new", "now", "old", "see", "way", "who", "did", "got", "let",
    "say", "she", "too", "use", "an", "as", "at", "be", "by", "do", "go",
    "he", "if", "in", "is", "it", "me", "my", "no", "of", "on", "or", "so",
    "to", "up", "we", "am", "an",
    # Technical common words
    "to", "into", "via", "per", "than", "then", "them", "they", "when",
    "where", "which", "while", "your", "you", "will", "would", "could",
    "should", "about", "after", "before", "between", "during", "through",
}


def compute_similarity(keywords_a: set, keywords_b: set) -> float:
    """Jaccard-like similarity, weighted by inverse frequency."""
    # Remove stop words
    a = keywords_a - STOP_WORDS
    b = keywords_b - STOP_WORDS
    
    if not a or not b:
        return 0.0
    
    intersection = a & b
    union = a | b
    
    if not union:
        return 0.0
    
    return len(intersection) / len(union)


def find_related(all_files: list, target_idx: int, top_n: int) -> list:
    """Find top-N related files for a given file index."""
    target_name = all_files[target_idx]["name"]
    target_kw = all_files[target_idx]["keywords"]
    
    scores = []
    for i, other in enumerate(all_files):
        if i == target_idx:
            continue
        sim = compute_similarity(target_kw, other["keywords"])
        if sim > 0.05:  # minimum threshold
            scores.append((sim, other["name"]))
    
    scores.sort(key=lambda x: (-x[0], x[1]))
    return scores[:top_n]


def update_see_also(path: Path, body: str, links: list, dry_run: bool) -> str:
    """Add or update ## See Also section in file body."""
    if not links:
        return "NO LINKS"
    
    see_also_lines = ["", "## See Also"]
    for score, name in links:
        see_also_lines.append(f"- [Related: {name}]({name}.md)")
    see_also_text = "\n".join(see_also_lines)
    
    # Check if body already has a ## See Also section
    lines = body.split("\n")
    see_also_start = None
    next_section_start = None
    
    for i, line in enumerate(lines):
        stripped = line.strip()
        if stripped.lower() == "## see also":
            see_also_start = i
        elif see_also_start is not None and stripped.startswith("## "):
            next_section_start = i
            break
    
    if see_also_start is not None:
        # Replace existing See Also section
        end = next_section_start if next_section_start is not None else len(lines)
        new_lines = lines[:see_also_start] + see_also_lines[1:] + lines[end:]
        new_body = "\n".join(new_lines).rstrip()
    else:
        # Append See Also at end
        new_body = body.rstrip() + see_also_text
    
    return new_body


def main():
    dry_run = "--dry-run" in sys.argv
    top_n = 3
    memory_dir = Path("/home/ubuntu/.myclaw/workspace/memory")
    
    for i, arg in enumerate(sys.argv):
        if arg == "--top-n" and i + 1 < len(sys.argv):
            top_n = int(sys.argv[i + 1])
        elif arg == "--memory-dir" and i + 1 < len(sys.argv):
            memory_dir = Path(sys.argv[i + 1])
    
    md_files = sorted(memory_dir.glob("*.md"))
    
    # Parse all files
    all_files = []
    for path in md_files:
        fm, body, raw = parse_file(path)
        name = fm.get("name", path.stem)
        keywords = extract_keywords(fm, name, body)
        all_files.append({
            "path": path,
            "name": name,
            "fm": fm,
            "body": body,
            "raw": raw,
            "keywords": keywords,
        })
    
    print(f"{'DRY RUN: ' if dry_run else ''}Generating See Also links for {len(all_files)} files")
    print(f"Top-N: {top_n}")
    print("=" * 70)
    
    updated = 0
    for idx, entry in enumerate(all_files):
        related = find_related(all_files, idx, top_n)
        if not related:
            print(f"  {entry['name']}: no related entries found")
            continue
        
        related_names = [f"{name}({score:.2f})" for score, name in related]
        print(f"  {entry['name']}: {', '.join(related_names)}")
        
        new_body = update_see_also(entry["path"], entry["body"], related, dry_run)
        if new_body != "NO LINKS" and new_body != entry["body"]:
            if not dry_run:
                # Reconstruct file with frontmatter + new body
                raw = entry["raw"]
                trimmed = raw.strip()
                rest = trimmed[3:].lstrip("\r\n")
                end = rest.find("\n---")
                fm_text = rest[:end]
                new_content = f"---\n{fm_text}\n---\n\n{new_body}\n"
                entry["path"].write_text(new_content, encoding="utf-8")
            updated += 1
    
    print("=" * 70)
    print(f"Total: {len(all_files)} | Updated: {updated}")


if __name__ == "__main__":
    main()
