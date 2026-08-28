#!/usr/bin/env python3
"""Migrate {base_dir}/skills to the operator's user layer (RFC #101 P1).

Layout: {base_dir}/skills/{name} -> {base_dir}/users/{operator}/skills/{name}
Directory-authoritative: SKILL.md frontmatter is never touched.

Note: base_dir (~/.myclaw) is daemon data, NOT a git repo — plain moves,
no `git mv`. Idempotent: re-running skips names already present on the
target. `--rollback` moves everything back the same way. Both directions
verify the skill-name inventory is conserved (every skill present before
must be present after; nothing extra may appear) and exit non-zero
otherwise, leaving a report on stdout.
"""
import os
import sys
import shutil
import argparse

OPERATOR_USER_ID = "01a0151d-997f-7980-9ad1-cd9caf893d87"


def skill_names(directory):
    """Skill names = subdirectories containing a SKILL.md."""
    if not os.path.isdir(directory):
        return set()
    return {
        name
        for name in os.listdir(directory)
        if os.path.isfile(os.path.join(directory, name, "SKILL.md"))
    }


def move_layer(src_dir, dst_dir):
    """Move every skill from src_dir to dst_dir, skipping name collisions.

    Returns (moved, skipped, leftovers) — leftovers are items in src_dir
    that are not skills (no SKILL.md) and are therefore left untouched.
    """
    os.makedirs(dst_dir, exist_ok=True)
    moved, skipped = [], []
    for item in sorted(os.listdir(src_dir)):
        s = os.path.join(src_dir, item)
        if not os.path.isdir(s) or not os.path.isfile(os.path.join(s, "SKILL.md")):
            continue
        d = os.path.join(dst_dir, item)
        if os.path.exists(d):
            print(f"  skip {item} (already exists on target)")
            skipped.append(item)
        else:
            shutil.move(s, d)
            print(f"  moved {item}")
            moved.append(item)
    leftovers = sorted(
        n for n in os.listdir(src_dir)
        if os.path.isdir(os.path.join(src_dir, n))
        and not os.path.isfile(os.path.join(src_dir, n, "SKILL.md"))
    )
    return moved, skipped, leftovers


def main():
    parser = argparse.ArgumentParser(description="Migrate skills to user layer")
    parser.add_argument("--rollback", action="store_true", help="Rollback migration")
    args = parser.parse_args()

    base_dir = os.environ.get("MYCLAW_BASE_DIR") or os.path.expanduser("~/.myclaw")
    old_skills_dir = os.path.join(base_dir, "skills")
    new_skills_dir = os.path.join(base_dir, "users", OPERATOR_USER_ID, "skills")

    if args.rollback:
        if not os.path.exists(new_skills_dir):
            print("No migrated skills to rollback.")
            return
        before = skill_names(new_skills_dir) | skill_names(old_skills_dir)
        print(f"Rolling back {new_skills_dir} -> {old_skills_dir}")
        moved, skipped, leftovers = move_layer(new_skills_dir, old_skills_dir)
        after = skill_names(old_skills_dir)
        # everything that existed anywhere before must exist in old dir after
        missing = before - after
        print(f"Rolled back {len(moved)} skills ({len(skipped)} skipped, "
              f"{len(leftovers)} non-skill dirs untouched).")
        if missing:
            print(f"VERIFY FAILED: skills lost during rollback: {sorted(missing)}",
                  file=sys.stderr)
            sys.exit(1)
        print("Inventory verified: no skill lost.")
        return

    if not os.path.exists(old_skills_dir):
        print("No old skills to migrate.")
        return
    # Idempotence-aware invariant: the union of skills visible anywhere
    # before the move must equal the target inventory after it (skills
    # migrated in an earlier run count as preexisting, not "extra").
    before = skill_names(old_skills_dir) | skill_names(new_skills_dir)
    print(f"Migrating {old_skills_dir} -> {new_skills_dir} ({len(skill_names(old_skills_dir))} skills)")
    moved, skipped, leftovers = move_layer(old_skills_dir, new_skills_dir)
    after = skill_names(new_skills_dir)
    missing = before - after
    extra = after - before
    print(f"Migrated {len(moved)} skills ({len(skipped)} skipped, "
          f"{len(leftovers)} non-skill dirs untouched).")
    if missing:
        print(f"VERIFY FAILED: skills lost during migration: {sorted(missing)}",
              file=sys.stderr)
        sys.exit(1)
    if extra:
        print(f"VERIFY FAILED: unexpected extra skills on target: {sorted(extra)}",
              file=sys.stderr)
        sys.exit(1)
    print("Inventory verified: skill set conserved.")


if __name__ == "__main__":
    main()
