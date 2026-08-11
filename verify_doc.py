import re, glob

DOC = '/home/ubuntu/.myclaw/workspace/docs/architecture.md'
SRC = '/home/ubuntu/.myclaw/workspace/MyClaw/src'

s = open(DOC).read()

# Grab every .rs path mentioned anywhere in the doc
mentioned = set(re.findall(r'`([a-zA-Z0-9_/]+\.rs)`', s))

# All actual source files (relative to src/)
src_files = sorted(glob.glob(SRC + '/**/*.rs', recursive=True))
src_rel = {p[len(SRC)+1:] for p in src_files}

missing = sorted(src_rel - mentioned)
extra   = sorted(mentioned - src_rel - {'src/lib.rs'})  # tolerate a few common strays

print("source files:", len(src_rel))
print("mentioned .rs:", len(mentioned))
print("MISSING from doc:", len(missing))
for m in missing:
    print("   MISS:", m)
print("EXTRA (mentioned but not in src):", len(extra))
for e in extra[:20]:
    print("   EXTRA:", e)

# Count detail sections: headings that look like file sections
# Try multiple heading styles
for pat in [r'^###\s+`([^`]+\.rs)`', r'^####\s+`([^`]+\.rs)`', r'^##\s+`([^`]+\.rs)`', r'^### (.+\.rs)$', r'^#### (.+\.rs)$', r'^## (.+\.rs)$']:
    found = re.findall(pat, s, re.M)
    print("pattern", repr(pat), "count", len(found))
