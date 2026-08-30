#!/usr/bin/env python3
"""Migrate the flat {base}/memory pool into agent/user layers per the P4 TSV
adjudication (RFC #101 P4 stage 3).

Input TSV (default {base}/memory-migration/migration-final.tsv): one row per
memory, columns `name \t final \t orig-scope \t orig-grade`. Only the first
two columns drive behavior; `final` is agent | user | deleted. The TSV is the
sole adjudication source — no counts are baked in anywhere.

Forward run:
  0. Precheck: TSV name set must equal the {base}/memory/*.md disk inventory
     exactly, and no TSV name may already exist under users/{uuid}/memory
     (guards against stacking a second run on top of a finished one).
  1. Backup every pool file to {base}/backups/memory-split-pre-migration/
     and write manifest.json there recording what this run owns
     (`owned`: all TSV names, stems) and what it does NOT own
     (`foreign_user_files`: user-layer .md filenames that predate the run),
     plus the TSV content hash (`tsv_sha256`) and `created_at`.
  2. Dispatch per TSV final: agent files stay (frontmatter scope normalized to
     unquoted `agent`, user_id line removed), user files move to
     users/{uuid}/memory (scope `user`, user_id set to the operator FQID).
     Deleted files: placeholder_check / tmp_view_placeholder are removed;
     wechat_mp_history_tracking_workaround's body is appended to
     wechat_mp_draft_period_tracking under a dated heading, then removed.
  3. Link rewrite across all surviving files (body incl. See Also, Rust
     extract/parse_md_link semantics): same-layer bare links stay bare,
     cross-layer links get an agent:/user: prefix, wrongly-prefixed links are
     corrected. Links whose target survived nowhere are de-linked in place —
     `[Label](old.md)` becomes `Label（<date> 迁移清理：目标 old 已移除）`,
     line and sibling links kept — so the zero-dangling invariant holds with
     no link syntax left behind. No target guessing, no repair.
  4. Absolute Invariants (all must pass for exit 0): count conservation,
     zero dangling links, layer/scope attribution consistency, and re-run
     idempotency (a finished pool re-detects as migrated -> full no-op).

--rollback restores {base}/memory from the backup and clears the user memory
layer — but ONLY for files this migration owns, per the backup manifest:
owned files are restored to memory/ and removed from the user layer, foreign
files (recorded in manifest.json) are never touched, and after rollback the
surviving foreign set is verified against the manifest. A forward run aborts
if a backup directory already exists with a missing/corrupt manifest.
--dry-run prints the full plan without touching disk. `.pending` files are
never created or consumed by this script.

{base} is daemon data, not a git repo — plain filesystem operations only.

Runtime exclusion (operational contract): mutating runs (forward migration
without --dry-run, and --rollback) refuse to start while the myclaw daemon
appears to be running. Probe order: `systemctl --user is-active myclaw`
first, `pgrep -f myclaw` as fallback (own pid excluded); active -> abort with
the stop command (`systemctl --user stop myclaw`). --force overrides with an
explicit risk warning. --dry-run is read-only and exempt. On success the
script prints the restart hint (`systemctl --user start myclaw`).
"""
import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import uuid
from datetime import date

# Default operator (uuid derived from the FQID, mirroring src/ids Fqid::parse).
DEFAULT_OPERATOR_FQID = "myclaw/u/01a0151d-997f-7980-9ad1-cd9caf893d87"
DEFAULT_NAMESPACE = "myclaw"
OPERATOR_FQID = DEFAULT_OPERATOR_FQID  # module-level default (pre-param compat)
OPERATOR_UUID = DEFAULT_OPERATOR_FQID.rsplit("/", 1)[-1]

DAEMON_NAME = "myclaw"
DAEMON_STOP_HINT = "systemctl --user stop myclaw"
DAEMON_START_HINT = "systemctl --user start myclaw"

BACKUP_DIRNAME = "memory-split-pre-migration"
MANIFEST_NAME = "manifest.json"

# The three adjudicated deletions (P4 TSV). Any other `deleted` row is an
# unimplemented adjudication and aborts the run.
DELETED_PLAIN = ("placeholder_check", "tmp_view_placeholder")
MERGE_SOURCE = "wechat_mp_history_tracking_workaround"
MERGE_TARGET = "wechat_mp_draft_period_tracking"

MD_LINK_RE = re.compile(r"\[([^\]]*)\]\(([^()]*)\)")


class MigrationError(Exception):
    """Abort condition — message goes to stderr, exit code 1."""


def fail(msg):
    raise MigrationError(msg)


# --------------------------------------------------------------------------- #
# low-level helpers
# --------------------------------------------------------------------------- #

def md_files(directory):
    if not os.path.isdir(directory):
        return []
    return sorted(
        p for p in os.listdir(directory)
        if p.endswith(".md") and os.path.isfile(os.path.join(directory, p))
    )


def md_names(directory):
    return {p[:-3] for p in md_files(directory)}


def read_file(path):
    with open(path, "r", encoding="utf-8") as f:
        return f.read()


def write_file(path, text):
    with open(path, "w", encoding="utf-8") as f:
        f.write(text)


def sha256_file(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(65536), b""):
            h.update(chunk)
    return h.hexdigest()


# --------------------------------------------------------------------------- #
# daemon runtime exclusion (operational contract)
# --------------------------------------------------------------------------- #

def default_command_runner(cmd):
    """Run cmd, return (returncode, stdout). Missing binary -> (127, '')."""
    try:
        p = subprocess.run(cmd, capture_output=True, text=True, timeout=30)
    except FileNotFoundError:
        return 127, ""
    except subprocess.TimeoutExpired:
        return 124, ""
    return p.returncode, p.stdout or ""


def daemon_active(runner=default_command_runner):
    """Is the myclaw daemon running? Prefer `systemctl --user is-active myclaw`
    (exit 0 == active); on any other systemctl outcome (inactive / failed /
    no user systemd / binary missing) fall back to `pgrep -f myclaw`.
    Returns (active, how) — `how` names the probe that decided, for the abort
    message. Our own pid is never counted (the script path may contain
    'myclaw', and pgrep matches full command lines)."""
    rc, _ = runner(["systemctl", "--user", "is-active", DAEMON_NAME])
    if rc == 0:
        return True, "systemctl --user is-active myclaw"
    rc, out = runner(["pgrep", "-f", DAEMON_NAME])
    if rc == 0:
        pids = {ln.strip() for ln in out.splitlines() if ln.strip().isdigit()}
        pids.discard(str(os.getpid()))
        if pids:
            return True, "pgrep -f myclaw"
    return False, None


def guard_daemon_stopped(runner, force, dry_run=False):
    """Mutating runs require a stopped daemon. --dry-run is read-only and
    exempt; --force overrides with an explicit risk warning."""
    if dry_run:
        return
    active, how = daemon_active(runner)
    if not active:
        return
    if force:
        print(f"WARNING: --force with daemon ACTIVE ({how}) — migrating a live "
              f"memory pool risks watcher races and lost writes. Proceeding anyway.")
        return
    fail(f"daemon appears ACTIVE ({how}) — the memory pool must not be mutated "
         f"under a running daemon. Stop it first: {DAEMON_STOP_HINT} "
         f"(or rerun with --force to accept the risk)")


def line_ending(line):
    if line.endswith("\r\n"):
        return "\r\n"
    if line.endswith("\n"):
        return "\n"
    return ""


def split_frontmatter(text):
    """Split `--- frontmatter ---` block. Returns (fm_lines, open, close, body)
    where fm_lines keep their line endings; (None, None, None, text) if the
    file has no frontmatter block."""
    lines = text.splitlines(keepends=True)
    if not lines or lines[0].strip() != "---":
        return None, None, None, text
    for i in range(1, len(lines)):
        if lines[i].strip() == "---":
            return lines[1:i], lines[0], lines[i], "".join(lines[i + 1:])
    return None, None, None, text


def fm_key(line):
    m = re.match(r"^\s*([A-Za-z0-9_]+)\s*:", line)
    return m.group(1).lower() if m else None


def parse_md_href(href):
    """Validate a markdown link href with the Rust parse_md_link semantics.
    Returns (layer_prefix_or_None, bare_stem) for canonical memory hrefs,
    else None (external anchor / bare name without .md / empty / '..'...)."""
    t = href.strip()
    if not t:
        return None
    if t.startswith(("http://", "https://", "#", "mailto:")):
        return None
    segment = re.split(r"[/\\]", t)[-1].strip()
    if not segment or segment == ".." or ".." in segment:
        return None
    m = re.match(r"^(.+)\.md$", segment, re.IGNORECASE)
    if not m:
        return None
    stem = m.group(1)
    if not stem:
        return None
    if stem.startswith("agent:") and len(stem) > len("agent:"):
        return "agent", stem[len("agent:"):]
    if stem.startswith("user:") and len(stem) > len("user:"):
        return "user", stem[len("user:"):]
    return None, stem


# --------------------------------------------------------------------------- #
# operator FQID
# --------------------------------------------------------------------------- #

def parse_operator_fqid(value):
    """Parse --operator-fqid, mirroring src/ids Fqid::parse semantics:
    `<namespace>/<type>/<uuid>` where namespace must be `myclaw` (default
    instance namespace) and the uuid is any form uuid.UUID accepts, returned
    canonicalized to lowercase-hyphenated 36 chars (Rust uuid_str() form).
    Tightened beyond Fqid::parse: the operator is a user, so the type segment
    must be `u`. Returns (canonical_fqid, uuid_str); invalid shape aborts."""
    parts = value.split("/")
    if len(parts) != 3 or not all(parts):
        fail(f"invalid --operator-fqid {value!r}: expected <namespace>/<u>/<uuid>, "
             f"e.g. {DEFAULT_OPERATOR_FQID}")
    ns, type_seg, uuid_str = parts
    if ns != DEFAULT_NAMESPACE:
        fail(f"invalid --operator-fqid {value!r}: namespace must be {DEFAULT_NAMESPACE!r}")
    if type_seg != "u":
        fail(f"invalid --operator-fqid {value!r}: type segment must be 'u' "
             f"(operator is a user id), got {type_seg!r}")
    try:
        parsed = uuid.UUID(uuid_str)
    except ValueError:
        fail(f"invalid --operator-fqid {value!r}: uuid segment {uuid_str!r} "
             f"is not a valid UUID")
    canonical = f"{ns}/{type_seg}/{parsed}"
    return canonical, str(parsed)


# --------------------------------------------------------------------------- #
# backup manifest
# --------------------------------------------------------------------------- #

def manifest_path(backup_dir):
    return os.path.join(backup_dir, MANIFEST_NAME)


def write_manifest(backup_dir, owned, foreign_user_files, tsv_sha256):
    manifest = {
        "version": 1,
        "created_at": date.today().isoformat(),
        "tsv_sha256": tsv_sha256,
        # memory names (stems) this run owns — every TSV row's file
        "owned": sorted(owned),
        # user-layer filenames this run does NOT own; never touched by it
        "foreign_user_files": sorted(foreign_user_files),
    }
    write_file(manifest_path(backup_dir),
               json.dumps(manifest, indent=2, ensure_ascii=False) + "\n")
    return manifest


def load_manifest(backup_dir):
    """Load and shape-validate the backup manifest; abort on any problem."""
    path = manifest_path(backup_dir)
    if not os.path.isfile(path):
        fail(f"backup dir exists but {MANIFEST_NAME} is missing: {path} — "
             f"cannot tell owned from foreign files. If the backup contains no "
             f".md files it is a leftover of an interrupted step 1: inspect and "
             f"remove it manually, otherwise recover by hand.")
    try:
        manifest = json.loads(read_file(path))
    except ValueError as e:
        fail(f"backup manifest is corrupt (invalid JSON): {path}: {e}")
    if not isinstance(manifest, dict):
        fail(f"backup manifest is corrupt (not a JSON object): {path}")
    for key in ("owned", "foreign_user_files", "tsv_sha256", "created_at"):
        if key not in manifest:
            fail(f"backup manifest is corrupt (missing {key!r}): {path}")
    for key in ("owned", "foreign_user_files"):
        if (not isinstance(manifest[key], list)
                or not all(isinstance(v, str) for v in manifest[key])):
            fail(f"backup manifest is corrupt ({key!r} must be a list of "
                 f"strings): {path}")
    return manifest


# --------------------------------------------------------------------------- #
# TSV
# --------------------------------------------------------------------------- #

def load_tsv(path):
    if not os.path.isfile(path):
        fail(f"TSV not found: {path}")
    final_by_name = {}
    with open(path, "r", encoding="utf-8") as f:
        for lineno, raw in enumerate(f, 1):
            line = raw.rstrip("\r\n")
            if not line.strip():
                continue
            cols = line.split("\t")
            if len(cols) < 2:
                fail(f"TSV line {lineno}: need at least 2 columns, got {len(cols)}: {line!r}")
            name, final = cols[0].strip(), cols[1].strip()
            if not name:
                fail(f"TSV line {lineno}: empty name")
            if final not in ("agent", "user", "deleted"):
                fail(f"TSV line {lineno}: final must be agent|user|deleted, got {final!r}")
            if name in final_by_name and final_by_name[name] != final:
                fail(f"TSV: conflicting finals for {name!r}: "
                     f"{final_by_name[name]} vs {final}")
            final_by_name[name] = final
    if not final_by_name:
        fail("TSV is empty")
    return final_by_name


class MigrationLock:
    """O_EXCL lock file (PR #212 review r3): at most one migration instance
    may mutate the tree at a time — guards against two concurrent runs both
    passing the fresh-state precheck and double-writing. Released via
    atexit (process exit covers every abort/return path); a crashed holder
    leaves a stale file whose message points at manual removal. --dry-run
    is exempt (read-only)."""

    def __init__(self, path):
        self.path = path
        self.fd = None

    def acquire(self):
        try:
            self.fd = os.open(self.path, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o644)
            os.write(self.fd, f"{os.getpid()}\n".encode())
        except FileExistsError:
            holder = "?"
            try:
                with open(self.path) as fh:
                    holder = fh.read().strip() or "?"
            except OSError:
                pass
            raise MigrationError(
                f"another migration appears to be running "
                f"(lock {self.path}, holder pid {holder}). If that process no "
                f"longer exists, delete the lock file and retry.")

    def release(self):
        if self.fd is not None:
            os.close(self.fd)
            self.fd = None
            try:
                os.remove(self.path)
            except OSError:
                pass


def validate_deleted_adjudication(final_by_name):
    deleted = {n for n, f in final_by_name.items() if f == "deleted"}
    known = set(DELETED_PLAIN) | {MERGE_SOURCE}
    unimplemented = deleted - known
    if unimplemented:
        fail(f"TSV contains deleted rows this script does not implement "
             f"(only {sorted(known)} are adjudicated): {sorted(unimplemented)}")
    if MERGE_SOURCE in deleted and final_by_name.get(MERGE_TARGET) in (None, "deleted"):
        fail(f"merge target {MERGE_TARGET!r} missing or deleted in TSV; cannot merge "
             f"{MERGE_SOURCE!r} into it")


# --------------------------------------------------------------------------- #
# frontmatter normalization & link rewrite
# --------------------------------------------------------------------------- #

def normalize_frontmatter(text, final, operator_fqid):
    """Normalize only scope / user_id lines (quote forms included); never
    reorder or touch other lines; preserve the file's newline style."""
    fm, opener, closer, body = split_frontmatter(text)
    if fm is None:
        fail("file has no frontmatter block")
    out, scope_idx, user_id_done = [], None, final != "user"
    for line in fm:
        key = fm_key(line)
        if key == "scope":
            out.append(f"scope: {final}" + line_ending(line))
            scope_idx = len(out) - 1
        elif key == "user_id":
            if final == "user":
                out.append(f'user_id: "{operator_fqid}"' + line_ending(line))
                user_id_done = True
            # agent layer: drop the user_id line entirely
        else:
            out.append(line)
    if scope_idx is None:
        out.insert(0, f"scope: {final}\n")
        scope_idx = 0
    if not user_id_done:
        out.insert(scope_idx + 1, f'user_id: "{operator_fqid}"\n')
    return opener + "".join(out) + closer + body


def href_for(stem, layer, src_layer):
    """Canonical href for target `stem` living in `layer`, seen from src_layer."""
    if layer == src_layer:
        return f"{stem}.md"
    return f"{layer}:{stem}.md"


def delink_text(label, target):
    """Plain-text replacement for a dead link: the label survives, the removal
    is noted with the migration date. No link syntax remains."""
    return f"{label}（{date.today().isoformat()} 迁移清理：目标 {target} 已移除）"


def rewrite_links(text, src_layer, surviving):
    """Rewrite/de-link memory links in the body (frontmatter untouched).
    `surviving` maps name -> final layer. Returns (new_text, rewrites, dead):
    rewrites = [(old_href, new_href)], dead = [dangling target (prefixed)].
    Dead links (target survived nowhere) are de-linked in place —
    `[Label](old.md)` becomes `Label（<date> 迁移清理：目标 old 已移除）` —
    anywhere in the body (See Also or inline prose): the line and any sibling
    links survive, and no dangling link syntax is left behind."""
    fm, opener, closer, body = split_frontmatter(text)
    if fm is None:
        fm, opener, closer, body = [], "", "", text
    out_lines, rewrites, dead = [], [], []
    for line in body.splitlines(keepends=True):
        edits = []  # (start, end, replacement) spans within this line
        for m in MD_LINK_RE.finditer(line):
            parsed = parse_md_href(m.group(2))
            if parsed is None:
                continue  # not a canonical memory link — leave untouched
            prefix, stem = parsed
            if stem not in surviving:
                dead.append(f"{prefix}:{stem}.md" if prefix else f"{stem}.md")
                edits.append((m.start(), m.end(),
                              delink_text(m.group(1), f"{prefix}:{stem}" if prefix else stem)))
                continue
            tgt_layer = surviving[stem]
            if prefix is None:
                # bare link: keep same-layer, qualify cross-layer
                if tgt_layer != src_layer:
                    edits.append((m.start(2), m.end(2), f"{tgt_layer}:{stem}.md"))
            elif prefix != tgt_layer:
                # wrong layer prefix: rewrite to the canonical form for the relation
                edits.append((m.start(2), m.end(2), href_for(stem, tgt_layer, src_layer)))
            # else: correct layer prefix (or same-layer bare) — kept as-is
        if edits:
            href_edits = [(s, e, r) for s, e, r in edits if r.endswith(".md")]
            # report only href rewrites; de-links are reported via `dead`
            old_text = line
            parts, last = [], 0
            for start, end, repl in edits:
                parts.append(line[last:start])
                parts.append(repl)
                last = end
            parts.append(line[last:])
            line = "".join(parts)
            for s, e, r in href_edits:
                rewrites.append((old_text[s:e].strip(), r))
        out_lines.append(line)
    return opener + "".join(fm) + closer + "".join(out_lines), rewrites, dead


def extract_dangling(text, src_name, src_layer, inventory):
    """Invariant-2 scanner: every canonical link (incl. layer-qualified) must
    resolve — target exists AND the qualified layer matches (a bare link only
    resolves same-layer, per RFC #101 §6.3)."""
    dangling = []
    _, _, _, body = split_frontmatter(text)
    for m in MD_LINK_RE.finditer(body):
        parsed = parse_md_href(m.group(2))
        if parsed is None:
            continue
        prefix, stem = parsed
        if stem not in inventory:
            dangling.append((src_name + ".md", f"{prefix}:{stem}.md" if prefix else f"{stem}.md"))
        elif prefix and inventory[stem] != prefix:
            dangling.append((src_name + ".md", f"{prefix}:{stem}.md (lives in {inventory[stem]} layer)"))
        elif prefix is None and inventory[stem] != src_layer:
            dangling.append((src_name + ".md", f"{stem}.md (bare link across layers)"))
    return dangling


# --------------------------------------------------------------------------- #
# state detection
# --------------------------------------------------------------------------- #

def detect_state(final_by_name, agent_dir, user_dir, backup_dir, manifest=None, tsv_sha256=None):
    """(state, reasons): fresh | migrated | partial. `migrated` = a re-run
    must be a full no-op. Owned state must equal the TSV expectation AND the
    foreign user-layer baseline must be unchanged since the backup manifest
    was written; anything else (including manifest/TSV drift) is partial."""
    if not os.path.isdir(backup_dir):
        return "fresh", []
    if manifest is None:
        return "partial", ["backup dir exists but manifest.json is missing/corrupt"]
    reasons = []
    expect_agent = {n for n, f in final_by_name.items() if f == "agent"}
    expect_user = {n for n, f in final_by_name.items() if f == "user"}
    deleted = {n for n, f in final_by_name.items() if f == "deleted"}
    owned = set(manifest["owned"])
    foreign_baseline = {f[:-3] for f in manifest["foreign_user_files"]}
    agent_set, user_set = md_names(agent_dir), md_names(user_dir)

    if owned != set(final_by_name):
        reasons.append("manifest owned set != current TSV name set "
                       "(backup was taken against a different TSV)")
    if tsv_sha256 is not None and manifest.get("tsv_sha256") != tsv_sha256:
        reasons.append("manifest tsv_sha256 != current TSV hash (TSV changed after the backup)")
    if agent_set != expect_agent:
        reasons.append(f"agent dir != final=agent set "
                       f"(missing={sorted(expect_agent - agent_set)} extra={sorted(agent_set - expect_agent)})")
    if user_set != expect_user | foreign_baseline:
        reasons.append(f"user dir != final=user set + manifest foreign baseline "
                       f"(unexpected={sorted(user_set - expect_user - foreign_baseline)} "
                       f"missing={sorted((expect_user | foreign_baseline) - user_set)})")
    if deleted & (agent_set | user_set):
        reasons.append(f"deleted files still on disk: {sorted(deleted & (agent_set | user_set))}")
    return ("migrated" if not reasons else "partial"), reasons


# --------------------------------------------------------------------------- #
# invariants
# --------------------------------------------------------------------------- #

def run_invariants(final_by_name, agent_dir, user_dir, backup_dir, pre_disk_count,
                   manifest, operator_fqid):
    """All four Absolute Invariants. Returns (rows, ok) — rows for the report
    table, ok=False aborts with details on stdout/stderr. Owned state is judged
    against the TSV expectation; foreign files against the manifest baseline."""
    expect_agent = {n for n, f in final_by_name.items() if f == "agent"}
    expect_user = {n for n, f in final_by_name.items() if f == "user"}
    deleted = {n for n, f in final_by_name.items() if f == "deleted"}
    foreign_baseline = {f[:-3] for f in manifest["foreign_user_files"]}

    # 1. count conservation (self-counted, nothing baked in)
    agent_set, user_set = md_names(agent_dir), md_names(user_dir)
    c1_errors = []
    if len(final_by_name) != pre_disk_count:
        c1_errors.append(f"TSV rows {len(final_by_name)} != pre-migration disk count {pre_disk_count}")
    if agent_set != expect_agent:
        c1_errors.append(f"agent dir != final=agent set; "
                         f"missing={sorted(expect_agent - agent_set)} extra={sorted(agent_set - expect_agent)}")
    if not expect_user <= user_set:
        c1_errors.append(f"user dir missing final=user files: {sorted(expect_user - user_set)}")
    if deleted & (agent_set | user_set):
        c1_errors.append(f"deleted files still on disk: {sorted(deleted & (agent_set | user_set))}")
    extra_user = user_set - expect_user
    if extra_user != foreign_baseline:
        c1_errors.append(f"user dir extras != manifest foreign baseline: "
                         f"unexpected={sorted(extra_user - foreign_baseline)} "
                         f"missing={sorted(foreign_baseline - extra_user)}")

    # 2. zero dangling links across the whole pool (both layers, all files)
    # owned files only — foreign user-layer files are not this run's output
    inventory = {n: "agent" for n in agent_set}
    inventory.update({n: "user" for n in user_set})
    dangling = []
    for layer, directory in (("agent", agent_dir), ("user", user_dir)):
        for fname in md_files(directory):
            if fname[:-3] not in final_by_name:
                continue  # foreign to this migration
            dangling += extract_dangling(read_file(os.path.join(directory, fname)),
                                         fname[:-3], layer, inventory)

    # 3. attribution consistency (owned files only)
    attr_errors = []
    for fname in md_files(agent_dir):
        fm, _, _, _ = split_frontmatter(read_file(os.path.join(agent_dir, fname)))
        for line in fm or []:
            key = fm_key(line)
            if key == "scope" and line.split(":", 1)[1].strip().strip('"') == "user":
                attr_errors.append(f"agent dir: {fname} has scope: user")
            if key == "user_id":
                attr_errors.append(f"agent dir: {fname} has user_id")
    for fname in md_files(user_dir):
        if fname[:-3] not in final_by_name:
            continue  # foreign to this migration
        fm, _, _, _ = split_frontmatter(read_file(os.path.join(user_dir, fname)))
        uid = next((line.split(":", 1)[1].strip().strip('"') for line in fm or []
                    if fm_key(line) == "user_id"), None)
        if uid != operator_fqid:
            attr_errors.append(f"user dir: {fname} user_id={uid!r} != operator FQID")

    # 4. idempotency — the finished pool must re-detect as fully migrated
    state, _ = detect_state(final_by_name, agent_dir, user_dir, backup_dir, manifest)

    rows = [
        ("1 count conservation", not c1_errors),
        (f"2 zero dead links (dangling={len(dangling)})", not dangling),
        ("3 attribution consistency", not attr_errors),
        ("4 idempotent re-run (state=migrated)", state == "migrated"),
    ]
    details = []
    details += [f"invariant1: {e}" for e in c1_errors]
    details += [f"invariant2: {src} -> {tgt}" for src, tgt in dangling]
    details += [f"invariant3: {e}" for e in attr_errors]
    if state != "migrated":
        details.append(f"invariant4: pool state after run is {state!r}, a re-run would not no-op")
    return rows, not details, details


# --------------------------------------------------------------------------- #
# rollback
# --------------------------------------------------------------------------- #

def do_rollback(agent_dir, user_dir, backup_dir):
    """Restore the flat pool from the backup, strictly manifest-scoped:
    only `owned` files are restored to memory/ and removed from the user
    layer; `foreign_user_files` are never touched, and the surviving foreign
    set is verified against the manifest afterwards."""
    if not os.path.isdir(backup_dir):
        print("No backup directory — nothing to rollback.")
        return 0
    manifest = load_manifest(backup_dir)
    owned = set(manifest["owned"])
    foreign_manifest = set(manifest["foreign_user_files"])  # filenames, .md kept

    backup_on_disk = md_names(backup_dir)
    if backup_on_disk != owned:
        fail(f"backup integrity: .md inventory != manifest owned set "
             f"(missing from backup: {sorted(owned - backup_on_disk)}; "
             f"not owned but present: {sorted(backup_on_disk - owned)})")

    # 1. restore owned files into the flat pool (remove only owned files there)
    os.makedirs(agent_dir, exist_ok=True)
    for name in sorted(owned):
        p = os.path.join(agent_dir, name + ".md")
        if os.path.isfile(p):
            os.remove(p)
    for name in sorted(owned):
        shutil.copy2(os.path.join(backup_dir, name + ".md"),
                     os.path.join(agent_dir, name + ".md"))

    # 2. clear the user layer of owned files ONLY — foreign files stay
    user_removed = 0
    if os.path.isdir(user_dir):
        for fname in md_files(user_dir):
            if fname[:-3] in owned:
                os.remove(os.path.join(user_dir, fname))
                user_removed += 1
        try:
            os.rmdir(user_dir)  # succeeds only when no foreign file remains
        except OSError:
            pass

    shutil.rmtree(backup_dir)

    # 3. verification
    restored = md_names(agent_dir)
    foreign_now = set(md_files(user_dir)) if os.path.isdir(user_dir) else set()
    problems = []
    if not owned <= restored:
        problems.append(f"owned files missing after restore: {sorted(owned - restored)}")
    missing_foreign = sorted(foreign_manifest - foreign_now)
    if missing_foreign:
        problems.append(f"foreign files lost (must never be touched): {missing_foreign}")
    extra_foreign = sorted(foreign_now - foreign_manifest)
    extra_agent = sorted(restored - owned)

    print(f"rollback: restored {len(owned)} owned files -> {agent_dir}; "
          f"removed {user_removed} owned files from user layer; backup deleted.")
    if extra_agent:
        print(f"note: {len(extra_agent)} non-owned files in memory/ were left in place: {extra_agent}")
    if extra_foreign:
        print(f"note: {len(extra_foreign)} user-layer files appeared after the backup "
              f"and were left untouched: {extra_foreign}")
    if problems:
        for p in problems:
            print(f"VERIFY FAILED: {p}", file=sys.stderr)
        return 1
    if foreign_now == foreign_manifest:
        print("rollback verified: restored inventory == backup owned set; "
              "foreign user files match the manifest.")
    return 0


# --------------------------------------------------------------------------- #
# forward migration
# --------------------------------------------------------------------------- #

def plan_and_execute(final_by_name, agent_dir, user_dir, backup_dir, dry_run,
                     operator_fqid, operator_uuid):
    deleted = {n for n, f in final_by_name.items() if f == "deleted"}
    surviving = {n: f for n, f in final_by_name.items() if f != "deleted"}

    counts = {"agent_stay": 0, "user_move": 0, "delete_plain": 0, "merge": 0,
              "fm_normalized": 0, "links_rewritten": 0, "dead_links_delinked": 0}
    dead_list, rewrite_list = [], []
    content = {}  # name -> post-dispatch text for survivors (drives step 3)

    def dst_path(name):
        d = agent_dir if final_by_name[name] == "agent" else user_dir
        return os.path.join(d, name + ".md")

    # -- step 2: dispatch (frontmatter normalize + move + delete + merge) ----
    for name in sorted(final_by_name):
        final = final_by_name[name]
        src = os.path.join(agent_dir, name + ".md")
        text = read_file(src)
        if final == "agent":
            new = normalize_frontmatter(text, "agent", operator_fqid)
            if new != text:
                counts["fm_normalized"] += 1
                rewrite_list.append((name, "frontmatter", "scope/user_id normalized"))
            content[name] = new
            counts["agent_stay"] += 1
            if not dry_run and new != text:
                write_file(src, new)
        elif final == "user":
            new = normalize_frontmatter(text, "user", operator_fqid)
            counts["fm_normalized"] += 1
            rewrite_list.append((name, "move", f"memory/ -> users/{operator_uuid}/memory/"))
            content[name] = new
            counts["user_move"] += 1
            if not dry_run:
                os.makedirs(user_dir, exist_ok=True)
                write_file(os.path.join(user_dir, name + ".md"), new)
                os.remove(src)
        else:  # deleted
            if name == MERGE_SOURCE:
                continue  # merged below, after the target has moved
            rewrite_list.append((name, "delete", "plain"))
            if not dry_run:
                os.remove(src)
            counts["delete_plain"] += 1

    # merge: append the workaround body to the target's END, then delete source.
    # The target is read from `content` (already dispatched/moved) so the merge
    # lands on the target's final location regardless of its TSV final layer.
    if MERGE_SOURCE in deleted:
        counts["merge"] += 1
        src = os.path.join(agent_dir, MERGE_SOURCE + ".md")
        _, _, _, body = split_frontmatter(read_file(src))
        heading = f"## 历史：旧版 workaround（{date.today().isoformat()} 迁移并入）"
        target_text = content[MERGE_TARGET]
        merged = target_text if target_text.endswith("\n") else target_text + "\n"
        merged += "\n" + heading + "\n\n" + body.strip("\n") + "\n"
        content[MERGE_TARGET] = merged
        rewrite_list.append((MERGE_SOURCE, "merge", f"body appended to {MERGE_TARGET}.md"))
        if not dry_run:
            write_file(dst_path(MERGE_TARGET), merged)
            os.remove(src)

    # -- step 3: link rewrite over every surviving file ----------------------
    for name in sorted(surviving):
        new, rewrites, dead = rewrite_links(content[name], surviving[name], surviving)
        content[name] = new  # post-rewrite text (de-linked dead links included)
        if dead:
            dead_list.extend((name, t) for t in dead)
            counts["dead_links_delinked"] += len(dead)
        if rewrites:
            rewrite_list.extend((name, "link", f"{old} -> {new}") for old, new in rewrites)
            counts["links_rewritten"] += len(rewrites)
        if not dry_run and content[name] != read_file(dst_path(name)):
            write_file(dst_path(name), content[name])

    return counts, dead_list, rewrite_list


def _build_parser():
    parser = argparse.ArgumentParser(
        description="P4 memory layer split: adjudicated flat pool -> agent/user layers")
    parser = argparse.ArgumentParser(
        description="Migrate the flat memory pool into agent/user layers per the P4 TSV")
    parser.add_argument("--base", default="~/.myclaw", help="myclaw base dir (default ~/.myclaw)")
    parser.add_argument("--tsv", default=None,
                        help="adjudication TSV (default {base}/memory-migration/migration-final.tsv)")
    parser.add_argument("--dry-run", action="store_true", help="print the plan, touch nothing")
    parser.add_argument("--rollback", action="store_true", help="restore the flat pool from the backup")
    parser.add_argument("--operator-fqid", default=DEFAULT_OPERATOR_FQID,
                        help=f"operator user FQID <ns>/u/<uuid> (default {DEFAULT_OPERATOR_FQID}); "
                             f"its uuid segment selects the users/{{uuid}}/memory target dir")
    parser.add_argument("--force", action="store_true",
                        help="proceed even if the myclaw daemon appears to be running (risky)")
    return parser


def main(argv=None, runner=None):
    """Single-process entry: take the migration lock (unless --dry-run) and
    hold it across the whole run so a concurrent instance fails closed."""
    gate = _build_parser().parse_args(argv)
    base = os.path.expanduser(gate.base)
    lock = None
    if not gate.dry_run:
        lock = MigrationLock(os.path.join(base, "memory-split-migration.lock"))
        try:
            lock.acquire()
        except MigrationError as e:
            print(f"ABORT: {e}", file=sys.stderr)
            return 1
    try:
        return _main_impl(argv, runner=runner)
    finally:
        if lock is not None:
            lock.release()


def _main_impl(argv=None, runner=None):
    # CLI entry passes runner=None meaning "really probe the system"; tests
    # inject make_runner(...). daemon_active has no None guard, so resolve
    # the default here (PR #212 review r3: CLI runs crashed in guard).
    if runner is None:
        runner = default_command_runner
    parser = _build_parser()
    args = parser.parse_args(argv)

    try:
        operator_fqid, operator_uuid = parse_operator_fqid(args.operator_fqid)
    except MigrationError as e:
        print(f"ABORT: {e}", file=sys.stderr)
        return 1

    base = os.path.expanduser(args.base)
    agent_dir = os.path.join(base, "memory")
    user_dir = os.path.join(base, "users", operator_uuid, "memory")
    backup_dir = os.path.join(base, "backups", BACKUP_DIRNAME)
    tsv_path = os.path.expanduser(args.tsv) if args.tsv else \
        os.path.join(base, "memory-migration", "migration-final.tsv")

    if args.rollback:
        try:
            guard_daemon_stopped(runner, args.force)
            return do_rollback(agent_dir, user_dir, backup_dir)
        except MigrationError as e:
            print(f"ABORT: {e}", file=sys.stderr)
            return 1

    try:
        final_by_name = load_tsv(tsv_path)
        validate_deleted_adjudication(final_by_name)
        tsv_sha256 = sha256_file(tsv_path)

        # a backup dir without a loadable manifest is unusable for both a
        # re-run and a later rollback — refuse up front (owned/foreign unknown)
        manifest = load_manifest(backup_dir) if os.path.isdir(backup_dir) else None

        state, reasons = detect_state(final_by_name, agent_dir, user_dir,
                                      backup_dir, manifest, tsv_sha256)
        if state == "migrated":
            print("Pool already migrated (backup present, layers match TSV, "
                  "foreign baseline unchanged) — no-op; invariants only.")
            rows, ok, details = run_invariants(final_by_name, agent_dir, user_dir,
                                               backup_dir, pre_disk_count=len(final_by_name),
                                               manifest=manifest, operator_fqid=operator_fqid)
            for label, passed in rows:
                print(f"  {'PASS' if passed else 'FAIL'}  {label}")
            for d in details:
                print(f"  {d}", file=sys.stderr)
            return 0 if ok else 1
        if state == "partial":
            fail("Backup exists but the pool is not in the fully-migrated state — "
                 "previous run interrupted, or the world changed since the backup: "
                 + "; ".join(reasons) + ". Run --rollback or inspect manually.")

        # -- step 0: precheck ------------------------------------------------
        disk = md_names(agent_dir)
        missing_on_disk = sorted(set(final_by_name) - disk)
        not_in_tsv = sorted(disk - set(final_by_name))
        if missing_on_disk or not_in_tsv:
            fail(f"precheck: TSV list != disk inventory "
                 f"(missing on disk: {missing_on_disk}; not in TSV: {not_in_tsv})")
        collisions = sorted(set(final_by_name) & md_names(user_dir))
        if collisions:
            fail(f"precheck: names already present in user layer (refusing to stack runs): "
                 f"{collisions}")
        foreign = md_names(user_dir) - set(final_by_name)
        pre_disk_count = len(disk)

        print(f"pool: {pre_disk_count} files | TSV: {len(final_by_name)} rows "
              f"(agent={sum(1 for f in final_by_name.values() if f == 'agent')}, "
              f"user={sum(1 for f in final_by_name.values() if f == 'user')}, "
              f"deleted={sum(1 for f in final_by_name.values() if f == 'deleted')})")

        if args.dry_run:
            counts, dead_list, rewrite_list = plan_and_execute(
                final_by_name, agent_dir, user_dir, backup_dir, dry_run=True,
                operator_fqid=operator_fqid, operator_uuid=operator_uuid)
            print(f"DRY RUN — nothing written. plan: {counts}")
            for name, t, detail in rewrite_list:
                print(f"  would {t:11s} {name}: {detail}")
            return 0

        # mutating run from here on: refuse to race a live daemon
        guard_daemon_stopped(runner, args.force, dry_run=args.dry_run)

        # -- step 1: backup + manifest -----------------------------------------
        if os.path.exists(backup_dir):
            fail(f"backup dir already exists: {backup_dir} — run --rollback or remove it first")
        os.makedirs(backup_dir)
        for fname in md_files(agent_dir):
            shutil.copy2(os.path.join(agent_dir, fname), os.path.join(backup_dir, fname))
        foreign = md_names(user_dir) - set(final_by_name)  # recompute at write time
        manifest = write_manifest(backup_dir, owned=set(final_by_name),
                                  foreign_user_files=sorted(f + ".md" for f in foreign),
                                  tsv_sha256=sha256_file(tsv_path))
        print(f"step 1 backup: {len(md_names(backup_dir))} files -> {backup_dir} "
              f"(owned={len(manifest['owned'])}, foreign_user_files={len(manifest['foreign_user_files'])})")

        # -- steps 2+3 ---------------------------------------------------------
        counts, dead_list, rewrite_list = plan_and_execute(
            final_by_name, agent_dir, user_dir, backup_dir, dry_run=False,
            operator_fqid=operator_fqid, operator_uuid=operator_uuid)
        print(f"step 2 dispatch: agent_stay={counts['agent_stay']} user_move={counts['user_move']} "
              f"delete_plain={counts['delete_plain']} merge={counts['merge']} "
              f"(frontmatter normalized on {counts['fm_normalized']})")
        print(f"step 3 links: rewritten={counts['links_rewritten']} dead_links_delinked={counts['dead_links_delinked']}")
        for src, tgt in dead_list:
            print(f"  dead link de-linked: {src}.md -> {tgt}")

        # shadow pairs: same name in both layers after migration (informational)
        shadow = sorted(md_names(agent_dir) & md_names(user_dir))
        print(f"cross-layer shadow pairs (same name in both layers): "
              f"{shadow if shadow else 'none'}")

        # -- step 4: Absolute Invariants ---------------------------------------
        rows, ok, details = run_invariants(final_by_name, agent_dir, user_dir,
                                           backup_dir, pre_disk_count, manifest=manifest,
                                           operator_fqid=operator_fqid)
        print("step 4 invariants:")
        for label, passed in rows:
            print(f"  {'PASS' if passed else 'FAIL'}  {label}")
        for d in details:
            print(f"  {d}", file=sys.stderr)
        if not ok:
            fail("invariants failed — pool left as-is after migration; use --rollback to restore")
        if foreign:
            print(f"note: {len(foreign)} pre-existing files in user layer were not in the TSV "
                  f"and were left untouched: {sorted(foreign)}")
        print("migration complete: all invariants pass.")
        print(f"restart the daemon when ready: {DAEMON_START_HINT}")
        return 0
    except MigrationError as e:
        print(f"ABORT: {e}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
