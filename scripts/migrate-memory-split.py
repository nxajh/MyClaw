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
  1. Backup every pool file to {base}/backups/memory-split-pre-migration/.
  2. Dispatch per TSV final: agent files stay (frontmatter scope normalized to
     unquoted `agent`, user_id line removed), user files move to
     users/{uuid}/memory (scope `user`, user_id set to the operator FQID).
     Deleted files: placeholder_check / tmp_view_placeholder are removed;
     wechat_mp_history_tracking_workaround's body is appended to
     wechat_mp_draft_period_tracking under a dated heading, then removed.
  3. Link rewrite across all surviving files (body incl. See Also, Rust
     extract/parse_md_link semantics): same-layer bare links stay bare,
     cross-layer links get an agent:/user: prefix, wrongly-prefixed links are
     corrected, links whose target survived nowhere get their whole line
     dropped (reported). No target guessing, no repair.
  4. Absolute Invariants (all must pass for exit 0): count conservation,
     zero dangling links, layer/scope attribution consistency, and re-run
     idempotency (a finished pool re-detects as migrated -> full no-op).

--rollback restores {base}/memory from the backup (cleared, then copied back),
empties users/{uuid}/memory, removes the backup, and verifies the restored
inventory equals the backup inventory. --dry-run prints the full plan without
touching disk. `.pending` files are never created or consumed by this script.

{base} is daemon data, not a git repo — plain filesystem operations only.
"""
import argparse
import os
import re
import shutil
import sys
from datetime import date

OPERATOR_UUID = "01a0151d-997f-7980-9ad1-cd9caf893d87"
OPERATOR_FQID = "myclaw/u/01a0151d-997f-7980-9ad1-cd9caf893d87"

BACKUP_DIRNAME = "memory-split-pre-migration"

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

def normalize_frontmatter(text, final):
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
                out.append(f'user_id: "{OPERATOR_FQID}"' + line_ending(line))
                user_id_done = True
            # agent layer: drop the user_id line entirely
        else:
            out.append(line)
    if scope_idx is None:
        out.insert(0, f"scope: {final}\n")
        scope_idx = 0
    if not user_id_done:
        out.insert(scope_idx + 1, f'user_id: "{OPERATOR_FQID}"\n')
    return opener + "".join(out) + closer + body


def href_for(stem, layer, src_layer):
    """Canonical href for target `stem` living in `layer`, seen from src_layer."""
    if layer == src_layer:
        return f"{stem}.md"
    return f"{layer}:{stem}.md"


def rewrite_links(text, src_layer, surviving):
    """Rewrite/drop memory links in the body (frontmatter untouched).
    `surviving` maps name -> final layer. Returns (new_text, rewrites, dead):
    rewrites = [(old_href, new_href)], dead = [dangling target (prefixed)]."""
    fm, opener, closer, body = split_frontmatter(text)
    if fm is None:
        fm, opener, closer, body = [], "", "", text
    out_lines, rewrites, dead = [], [], []
    for line in body.splitlines(keepends=True):
        decisions, line_dead = [], False
        for m in MD_LINK_RE.finditer(line):
            parsed = parse_md_href(m.group(2))
            if parsed is None:
                continue  # not a canonical memory link — leave untouched
            prefix, stem = parsed
            if stem not in surviving:
                dead.append(f"{prefix}:{stem}.md" if prefix else f"{stem}.md")
                line_dead = True
                continue
            tgt_layer = surviving[stem]
            if prefix is None:
                # bare link: keep same-layer, qualify cross-layer
                if tgt_layer != src_layer:
                    decisions.append((m, f"{tgt_layer}:{stem}.md"))
            elif prefix != tgt_layer:
                # wrong layer prefix: rewrite to the canonical form for the relation
                decisions.append((m, href_for(stem, tgt_layer, src_layer)))
            # else: correct layer prefix (or same-layer bare) — kept as-is
        if line_dead:
            continue  # drop the whole link line
        if decisions:
            rewrites.extend((m.group(2).strip(), want) for m, want in decisions)
            parts, last = [], 0
            for m, want in decisions:
                parts.append(line[last:m.start(2)])
                parts.append(want)
                last = m.end(2)
            parts.append(line[last:])
            line = "".join(parts)
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

def detect_state(final_by_name, agent_dir, user_dir, backup_dir):
    """fresh | migrated | partial. `migrated` = a re-run must be a full no-op."""
    if not os.path.isdir(backup_dir):
        return "fresh"
    expect_agent = {n for n, f in final_by_name.items() if f == "agent"}
    expect_user = {n for n, f in final_by_name.items() if f == "user"}
    agent_set, user_set = md_names(agent_dir), md_names(user_dir)
    deleted = {n for n, f in final_by_name.items() if f == "deleted"}
    if (agent_set == expect_agent
            and user_set >= expect_user
            and not (deleted & (agent_set | user_set))):
        return "migrated"
    return "partial"


# --------------------------------------------------------------------------- #
# invariants
# --------------------------------------------------------------------------- #

def run_invariants(final_by_name, agent_dir, user_dir, backup_dir, pre_disk_count):
    """All four Absolute Invariants. Returns (rows, ok) — rows for the report
    table, ok=False aborts with details on stdout/stderr."""
    expect_agent = {n for n, f in final_by_name.items() if f == "agent"}
    expect_user = {n for n, f in final_by_name.items() if f == "user"}
    deleted = {n for n, f in final_by_name.items() if f == "deleted"}

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

    # 2. zero dangling links across the whole pool (both layers, all files)
    inventory = {n: "agent" for n in agent_set}
    inventory.update({n: "user" for n in user_set})
    dangling = []
    for layer, directory in (("agent", agent_dir), ("user", user_dir)):
        for fname in md_files(directory):
            dangling += extract_dangling(read_file(os.path.join(directory, fname)),
                                         fname[:-3], layer, inventory)

    # 3. attribution consistency
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
        fm, _, _, _ = split_frontmatter(read_file(os.path.join(user_dir, fname)))
        uid = next((line.split(":", 1)[1].strip().strip('"') for line in fm or []
                    if fm_key(line) == "user_id"), None)
        if uid != OPERATOR_FQID:
            attr_errors.append(f"user dir: {fname} user_id={uid!r} != operator FQID")

    # 4. idempotency — the finished pool must re-detect as fully migrated
    state = detect_state(final_by_name, agent_dir, user_dir, backup_dir)

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
    if not os.path.isdir(backup_dir):
        print("No backup directory — nothing to rollback.")
        return 0
    backup_names = md_names(backup_dir)
    os.makedirs(agent_dir, exist_ok=True)
    # restore the flat pool: clear *.md, copy the backup back
    for fname in md_files(agent_dir):
        os.remove(os.path.join(agent_dir, fname))
    for name in sorted(backup_names):
        shutil.copy2(os.path.join(backup_dir, name + ".md"),
                     os.path.join(agent_dir, name + ".md"))
    # empty the user memory layer
    user_removed = 0
    if os.path.isdir(user_dir):
        for fname in md_files(user_dir):
            os.remove(os.path.join(user_dir, fname))
            user_removed += 1
        try:
            os.rmdir(user_dir)
        except OSError:
            pass
    shutil.rmtree(backup_dir)
    restored = md_names(agent_dir)
    print(f"rollback: restored {len(restored)} files -> {agent_dir}; "
          f"removed {user_removed} files from user layer; backup deleted.")
    if restored != backup_names:
        print(f"VERIFY FAILED: restored inventory != backup inventory: "
              f"missing={sorted(backup_names - restored)} extra={sorted(restored - backup_names)}",
              file=sys.stderr)
        return 1
    print("rollback verified: disk inventory == backup inventory.")
    return 0


# --------------------------------------------------------------------------- #
# forward migration
# --------------------------------------------------------------------------- #

def plan_and_execute(final_by_name, agent_dir, user_dir, backup_dir, dry_run):
    deleted = {n for n, f in final_by_name.items() if f == "deleted"}
    surviving = {n: f for n, f in final_by_name.items() if f != "deleted"}

    counts = {"agent_stay": 0, "user_move": 0, "delete_plain": 0, "merge": 0,
              "fm_normalized": 0, "links_rewritten": 0, "dead_link_lines": 0}
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
            new = normalize_frontmatter(text, "agent")
            if new != text:
                counts["fm_normalized"] += 1
                rewrite_list.append((name, "frontmatter", "scope/user_id normalized"))
            content[name] = new
            counts["agent_stay"] += 1
            if not dry_run and new != text:
                write_file(src, new)
        elif final == "user":
            new = normalize_frontmatter(text, "user")
            counts["fm_normalized"] += 1
            rewrite_list.append((name, "move", f"memory/ -> users/{OPERATOR_UUID}/memory/"))
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
        merged += f"\n{heading}\n\n{body.strip('\n')}\n"
        content[MERGE_TARGET] = merged
        rewrite_list.append((MERGE_SOURCE, "merge", f"body appended to {MERGE_TARGET}.md"))
        if not dry_run:
            write_file(dst_path(MERGE_TARGET), merged)
            os.remove(src)

    # -- step 3: link rewrite over every surviving file ----------------------
    for name in sorted(surviving):
        new, rewrites, dead = rewrite_links(content[name], surviving[name], surviving)
        content[name] = new  # always keep the post-rewrite text (drops included)
        if dead:
            dead_list.extend((name, t) for t in dead)
            counts["dead_link_lines"] += len(dead)
        if rewrites:
            rewrite_list.extend((name, "link", f"{old} -> {new}") for old, new in rewrites)
            counts["links_rewritten"] += len(rewrites)
        if not dry_run and content[name] != read_file(dst_path(name)):
            write_file(dst_path(name), content[name])

    return counts, dead_list, rewrite_list


def main(argv=None):
    parser = argparse.ArgumentParser(
        description="Migrate the flat memory pool into agent/user layers per the P4 TSV")
    parser.add_argument("--base", default="~/.myclaw", help="myclaw base dir (default ~/.myclaw)")
    parser.add_argument("--tsv", default=None,
                        help="adjudication TSV (default {base}/memory-migration/migration-final.tsv)")
    parser.add_argument("--dry-run", action="store_true", help="print the plan, touch nothing")
    parser.add_argument("--rollback", action="store_true", help="restore the flat pool from the backup")
    args = parser.parse_args(argv)

    base = os.path.expanduser(args.base)
    agent_dir = os.path.join(base, "memory")
    user_dir = os.path.join(base, "users", OPERATOR_UUID, "memory")
    backup_dir = os.path.join(base, "backups", BACKUP_DIRNAME)
    tsv_path = os.path.expanduser(args.tsv) if args.tsv else \
        os.path.join(base, "memory-migration", "migration-final.tsv")

    if args.rollback:
        return do_rollback(agent_dir, user_dir, backup_dir)

    try:
        final_by_name = load_tsv(tsv_path)
        validate_deleted_adjudication(final_by_name)

        state = detect_state(final_by_name, agent_dir, user_dir, backup_dir)
        if state == "migrated":
            print("Pool already migrated (backup present, layers match TSV) — no-op; invariants only.")
            rows, ok, details = run_invariants(final_by_name, agent_dir, user_dir,
                                               backup_dir, pre_disk_count=len(final_by_name))
            for label, passed in rows:
                print(f"  {'PASS' if passed else 'FAIL'}  {label}")
            for d in details:
                print(f"  {d}", file=sys.stderr)
            return 0 if ok else 1
        if state == "partial":
            fail("Backup exists but the pool is not in the fully-migrated state — "
                 "previous run was interrupted. Run with --rollback or inspect manually.")

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
                final_by_name, agent_dir, user_dir, backup_dir, dry_run=True)
            print(f"DRY RUN — nothing written. plan: {counts}")
            for name, t, detail in rewrite_list:
                print(f"  would {t:11s} {name}: {detail}")
            return 0

        # -- step 1: backup ----------------------------------------------------
        if os.path.exists(backup_dir):
            fail(f"backup dir already exists: {backup_dir} — run --rollback or remove it first")
        os.makedirs(backup_dir)
        for fname in md_files(agent_dir):
            shutil.copy2(os.path.join(agent_dir, fname), os.path.join(backup_dir, fname))
        print(f"step 1 backup: {len(md_names(backup_dir))} files -> {backup_dir}")

        # -- steps 2+3 ---------------------------------------------------------
        counts, dead_list, rewrite_list = plan_and_execute(
            final_by_name, agent_dir, user_dir, backup_dir, dry_run=False)
        print(f"step 2 dispatch: agent_stay={counts['agent_stay']} user_move={counts['user_move']} "
              f"delete_plain={counts['delete_plain']} merge={counts['merge']} "
              f"(frontmatter normalized on {counts['fm_normalized']})")
        print(f"step 3 links: rewritten={counts['links_rewritten']} dead_link_lines_dropped={counts['dead_link_lines']}")
        for src, tgt in dead_list:
            print(f"  dead link removed: {src}.md -> {tgt}")

        # shadow pairs: same name in both layers after migration (informational)
        shadow = sorted(md_names(agent_dir) & md_names(user_dir))
        print(f"cross-layer shadow pairs (same name in both layers): "
              f"{shadow if shadow else 'none'}")

        # -- step 4: Absolute Invariants ---------------------------------------
        rows, ok, details = run_invariants(final_by_name, agent_dir, user_dir,
                                           backup_dir, pre_disk_count)
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
        return 0
    except MigrationError as e:
        print(f"ABORT: {e}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
