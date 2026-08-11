import re, glob
s = open('/home/ubuntu/.myclaw/workspace/docs/architecture.md').read()
# headings appear as ### `path.rs` (path relative to src/, no "src/" prefix)
paths = re.findall(r'###\s+`([^`]+\.rs)`', s)
# normalize source paths to relative-to-src
src = [x[len('/home/ubuntu/.myclaw/workspace/MyClaw/src/'):] for x in glob.glob('/home/ubuntu/.myclaw/workspace/MyClaw/src/**/*.rs', recursive=True)]
missing = [f for f in src if f not in paths]
extra = [p for p in set(paths) if p not in set(src)]
print("### .rs headings:", len(paths), "unique:", len(set(paths)))
print("source .rs files:", len(src))
print("missing from doc:", len(missing))
for m in missing:
    print("  MISS:", m)
print("extra in doc:", len(extra))
for e in extra:
    print("  EXTRA:", e)
