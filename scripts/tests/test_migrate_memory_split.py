#!/usr/bin/env python3
# migrate-memory-split.py fixture 自测：临时目录小池 -> 三路分派/链接改写/死链删除/
# frontmatter 规整/四条 Invariants 故意违反/幂等重跑/rollback 还原/预检差集/dry-run。
# 运行：python3 -m pytest scripts/tests/test_migrate_memory_split.py -q
# （或 python3 scripts/tests/test_migrate_memory_split.py，unittest 兼容）
# 池全部自造，不触碰生产 ~/.myclaw。

import contextlib
import hashlib
import importlib.util
import io
import json
import sys
import tempfile
import unittest
from pathlib import Path

_SCRIPT = Path(__file__).resolve().parents[1] / "migrate-memory-split.py"
_spec = importlib.util.spec_from_file_location("migrate_memory_split", _SCRIPT)
M = importlib.util.module_from_spec(_spec)
sys.modules["migrate_memory_split"] = M
_spec.loader.exec_module(M)

FQID = M.OPERATOR_FQID
USER_DIR_REL = Path("users") / M.OPERATOR_UUID / "memory"


def md(name, scope="user", extra_fm=None, body="", quoted_scope=False):
    scope_line = f'scope: "{scope}"' if quoted_scope else f"scope: {scope}"
    fm = ["---", f'name: "{name}"', scope_line]
    fm += extra_fm if extra_fm is not None else []
    fm.append("---")
    return "\n".join(fm) + "\n" + body


class MigrateMemorySplitTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.base = Path(self.tmp.name)
        (self.base / "memory").mkdir(parents=True)
        (self.base / "memory-migration").mkdir(parents=True)

    def tearDown(self):
        self.tmp.cleanup()

    # ------------------------------------------------------------------ pool

    def write_pool(self, merge_target_final="user"):
        mem = self.base / "memory"
        uid = [f'user_id: "{FQID}"']

        (mem / "alpha_agent.md").write_text(md(
            "alpha_agent", scope="user", quoted_scope=True, extra_fm=uid + [
                'type: "project"', "inject: always", 'created_at: "2026-08-01"'],
            body="Alpha body.\n\n## See Also\n- [Beta](beta_user.md)\n"))

        (mem / "beta_user.md").write_text(md(
            "beta_user", scope="agent", extra_fm=["type: feedback", "inject: search"],
            body="Beta body.\n"))

        (mem / "gamma_user.md").write_text(md(
            "gamma_user", scope="agent",
            body="Gamma body.\n\n## See Also\n"
                 "- [Same layer](beta_user.md)\n"
                 "- [Cross layer](alpha_agent.md)\n"
                 "- [Prefixed ok](user:beta_user.md)\n"
                 "- [Wrong prefix](agent:beta_user.md)\n"
                 "- [Wrong cross](user:alpha_agent.md)\n"
                 "- [Dead](nonexistent_thing.md)\n"
                 "- [To deleted](placeholder_check.md)\n"
                 "- [External](https://example.com/x.md)\n"
                 "- [Bare name](beta_user)\n"))

        (mem / "placeholder_check.md").write_text(md("placeholder_check", body="ph\n"))
        (mem / "tmp_view_placeholder.md").write_text(md("tmp_view_placeholder", body="tmp\n"))

        (mem / "wechat_mp_history_tracking_workaround.md").write_text(md(
            "wechat_mp_history_tracking_workaround", body=(
                "Workaround body line.\n\n## See Also\n- [Beta](beta_user.md)\n")))
        (mem / "wechat_mp_draft_period_tracking.md").write_text(md(
            "wechat_mp_draft_period_tracking", body="Draft tracking body.\n"))

        rows = [
            ("alpha_agent", "agent", "user", "B"),
            ("beta_user", "user", "agent", "A"),
            ("gamma_user", "user", "agent", "A"),
            ("placeholder_check", "deleted", "user", "B"),
            ("tmp_view_placeholder", "deleted", "user", "B"),
            ("wechat_mp_history_tracking_workaround", "deleted", "user", "C"),
            ("wechat_mp_draft_period_tracking", merge_target_final, "user", "A"),
        ]
        (self.base / "memory-migration" / "migration-final.tsv").write_text(
            "".join("\t".join(r) + "\n" for r in rows))

    def run_script(self, *extra):
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            rc = M.main(["--base", str(self.base), *extra])
        return rc, out.getvalue(), err.getvalue()

    def snapshot(self):
        snap = {}
        for sub in ("memory", str(USER_DIR_REL)):
            d = self.base / sub
            if d.is_dir():
                for p in sorted(d.glob("*.md")):
                    snap[str(p.relative_to(self.base))] = p.read_text()
        return snap

    # ------------------------------------------------------- happy path

    def test_three_way_dispatch(self):
        self.write_pool()
        rc, out, err = self.run_script()
        self.assertEqual(rc, 0, out + err)
        mem, udir = self.base / "memory", self.base / USER_DIR_REL

        # agent stays: scope normalized unquoted, user_id dropped, rest intact
        alpha = (mem / "alpha_agent.md").read_text()
        self.assertIn("scope: agent\n", alpha)
        self.assertNotIn("scope: \"agent\"", alpha)
        self.assertNotIn("user_id", alpha)
        for kept in ('name: "alpha_agent"', 'type: "project"', "inject: always",
                     'created_at: "2026-08-01"'):
            self.assertIn(kept, alpha)

        # user moves: scope user + operator user_id added
        beta = (udir / "beta_user.md").read_text()
        self.assertIn("scope: user\n", beta)
        self.assertIn(f'user_id: "{FQID}"\n', beta)
        self.assertFalse((mem / "beta_user.md").exists())

        # deleted: plain gone everywhere, source merged into target's END
        for name in ("placeholder_check", "tmp_view_placeholder",
                     "wechat_mp_history_tracking_workaround"):
            self.assertFalse((mem / f"{name}.md").exists())
            self.assertFalse((udir / f"{name}.md").exists())
        target = (udir / "wechat_mp_draft_period_tracking.md").read_text()
        self.assertIn("## 历史：旧版 workaround（", target)
        self.assertIn("Workaround body line.", target)
        self.assertTrue(target.rstrip().endswith("- [Beta](beta_user.md)"))
        self.assertIn("Draft tracking body.", target)

        # backup keeps pre-migration originals (still scope "user" on alpha)
        backup = self.base / "backups" / M.BACKUP_DIRNAME
        self.assertIn('scope: "user"', (backup / "alpha_agent.md").read_text())

        self.assertIn("step 2 dispatch: agent_stay=1 user_move=3 delete_plain=2 merge=1", out)
        for inv in ("1 count conservation", "2 zero dead links",
                    "3 attribution consistency", "4 idempotent re-run"):
            self.assertIn(f"PASS  {inv}", out)
        self.assertIn("migration complete", out)

    def test_link_rewriting(self):
        self.write_pool()
        rc, out, _ = self.run_script()
        self.assertEqual(rc, 0)
        gamma = (self.base / USER_DIR_REL / "gamma_user.md").read_text()

        self.assertIn("- [Same layer](beta_user.md)", gamma)          # same layer -> bare
        self.assertIn("- [Cross layer](agent:alpha_agent.md)", gamma)  # cross layer -> prefixed
        self.assertIn("- [Prefixed ok](user:beta_user.md)", gamma)    # correct prefix kept as-is
        self.assertIn("- [Wrong prefix](beta_user.md)", gamma)        # wrong prefix -> canonical bare (same layer)
        self.assertNotIn("agent:beta_user.md", gamma)
        self.assertIn("- [Wrong cross](agent:alpha_agent.md)", gamma)  # wrong prefix -> corrected layer
        self.assertNotIn("nonexistent_thing.md", gamma)               # dead line dropped
        self.assertNotIn("placeholder_check.md", gamma)               # link to deleted dropped
        self.assertIn("- [External](https://example.com/x.md)", gamma)  # external untouched
        self.assertIn("- [Bare name](beta_user)", gamma)              # non-canonical untouched

        # alpha (agent layer) points at beta across layers
        alpha = (self.base / "memory" / "alpha_agent.md").read_text()
        self.assertIn("- [Beta](user:beta_user.md)", alpha)

        self.assertIn("dead link removed: gamma_user.md -> nonexistent_thing.md", out)
        self.assertIn("dead link removed: gamma_user.md -> placeholder_check.md", out)
        # merged-in section links get the same treatment (user->user stays bare)
        target = (self.base / USER_DIR_REL / "wechat_mp_draft_period_tracking.md").read_text()
        self.assertIn("- [Beta](beta_user.md)", target)

    def test_merge_target_in_agent_layer(self):
        # production TSV shape: merge target final=agent
        self.write_pool(merge_target_final="agent")
        rc, out, err = self.run_script()
        self.assertEqual(rc, 0, out + err)
        target = (self.base / "memory" / "wechat_mp_draft_period_tracking.md").read_text()
        self.assertIn("## 历史：旧版 workaround（", target)
        self.assertIn("scope: agent", target)

    # ------------------------------------------- deliberate invariant violations

    def test_invariant1_violation_precheck_aborts(self):
        self.write_pool()
        (self.base / "memory" / "gamma_user.md").unlink()  # TSV row without disk file
        rc, out, err = self.run_script()
        self.assertEqual(rc, 1)
        self.assertIn("TSV list != disk inventory", err)
        self.assertIn("gamma_user", err)
        # nothing mutated: no backup, no user layer, flat pool untouched
        self.assertFalse((self.base / "backups").exists())
        self.assertFalse((self.base / USER_DIR_REL).exists())
        self.assertTrue((self.base / "memory" / "beta_user.md").exists())

    def test_invariant2_dead_link_after_migration_aborts(self):
        self.write_pool()
        rc, _, _ = self.run_script()
        self.assertEqual(rc, 0)
        p = self.base / USER_DIR_REL / "beta_user.md"
        p.write_text(p.read_text() + "\n- [Dangling](ghost.md)\n")
        rc, out, err = self.run_script()
        self.assertEqual(rc, 1)
        self.assertIn("FAIL  2 zero dead links", out)
        self.assertIn("beta_user.md -> ghost.md", err)

    def test_invariant3_attribution_after_migration_aborts(self):
        self.write_pool()
        rc, _, _ = self.run_script()
        self.assertEqual(rc, 0)
        p = self.base / "memory" / "alpha_agent.md"
        p.write_text(p.read_text().replace("scope: agent\n", "scope: user\n"))
        rc, out, err = self.run_script()
        self.assertEqual(rc, 1)
        self.assertIn("FAIL  3 attribution consistency", out)
        self.assertIn("alpha_agent.md has scope: user", err)

    def test_invariant4_non_idempotent_state_aborts(self):
        self.write_pool()
        rc, _, _ = self.run_script()
        self.assertEqual(rc, 0)
        (self.base / "memory" / "alpha_agent.md").unlink()  # interrupted-pool shape
        rc, out, err = self.run_script()
        self.assertEqual(rc, 1)
        self.assertIn("not in the fully-migrated state", err)

    # ------------------------------------------------------- idempotency/rollback

    def test_idempotent_rerun_is_noop(self):
        self.write_pool()
        rc, _, _ = self.run_script()
        self.assertEqual(rc, 0)
        before = self.snapshot()
        rc, out, err = self.run_script()
        self.assertEqual(rc, 0, out + err)
        self.assertIn("no-op", out)
        self.assertEqual(self.snapshot(), before)  # byte-identical after re-run

    def test_rollback_restores_flat_pool(self):
        self.write_pool()
        original = {p.name: p.read_text() for p in (self.base / "memory").glob("*.md")}
        rc, _, _ = self.run_script()
        self.assertEqual(rc, 0)
        rc, out, err = self.run_script("--rollback")
        self.assertEqual(rc, 0, out + err)
        restored = {p.name: p.read_text() for p in (self.base / "memory").glob("*.md")}
        self.assertEqual(restored, original)  # byte-identical restore
        self.assertFalse((self.base / USER_DIR_REL / "beta_user.md").exists())
        self.assertFalse((self.base / "backups" / M.BACKUP_DIRNAME).exists())
        self.assertIn("rollback verified", out)

    # ------------------------------------------------------- dry-run / misc

    def test_dry_run_touches_nothing(self):
        self.write_pool()
        before = self.snapshot()
        rc, out, err = self.run_script("--dry-run")
        self.assertEqual(rc, 0, out + err)
        self.assertIn("DRY RUN — nothing written", out)
        self.assertIn("would move", out)
        self.assertEqual(self.snapshot(), before)
        self.assertFalse((self.base / "backups").exists())
        self.assertFalse((self.base / USER_DIR_REL).exists())

    def test_second_stack_run_aborts_on_user_layer_collision(self):
        # simulate a half-done world: file already in user layer, no backup
        self.write_pool()
        udir = self.base / USER_DIR_REL
        udir.mkdir(parents=True)
        (udir / "beta_user.md").write_text("---\nname: beta_user\n---\nx\n")
        rc, out, err = self.run_script()
        self.assertEqual(rc, 1)
        self.assertIn("already present in user layer", err)

    def test_unimplemented_deleted_row_aborts(self):
        self.write_pool()
        tsv = self.base / "memory-migration" / "migration-final.tsv"
        tsv.write_text(tsv.read_text() + "mystery_entry\tdeleted\tuser\tB\n")
        (self.base / "memory" / "mystery_entry.md").write_text(md("mystery_entry", body="m\n"))
        rc, out, err = self.run_script()
        self.assertEqual(rc, 1)
        self.assertIn("does not implement", err)
        self.assertIn("mystery_entry", err)

    # ------------------------------------------------- backup manifest / rollback safety

    def test_backup_manifest_contents(self):
        self.write_pool()
        rc, out, err = self.run_script()
        self.assertEqual(rc, 0, out + err)
        manifest = json.loads((self.base / "backups" / M.BACKUP_DIRNAME /
                               "manifest.json").read_text())
        self.assertEqual(manifest["owned"], sorted([
            "alpha_agent", "beta_user", "gamma_user", "placeholder_check",
            "tmp_view_placeholder", "wechat_mp_history_tracking_workaround",
            "wechat_mp_draft_period_tracking"]))
        self.assertEqual(manifest["foreign_user_files"], [])
        tsv_bytes = (self.base / "memory-migration" / "migration-final.tsv").read_bytes()
        self.assertEqual(manifest["tsv_sha256"], hashlib.sha256(tsv_bytes).hexdigest())
        self.assertTrue(manifest["created_at"])

    def test_rollback_preserves_foreign_user_files(self):
        self.write_pool()
        udir = self.base / USER_DIR_REL
        udir.mkdir(parents=True)
        (udir / "someone_elses_memory.md").write_text(
            '---\nname: someone_elses_memory\nscope: user\n---\nforeign body\n')
        (udir / "another_foreign.md").write_text("foreign 2\n")
        rc, _, _ = self.run_script()
        self.assertEqual(rc, 0)
        self.assertIn("someone_elses_memory", (udir / "someone_elses_memory.md").read_text())
        rc, out, err = self.run_script("--rollback")
        self.assertEqual(rc, 0, out + err)
        # foreign files: byte-identical, never touched by migration or rollback
        self.assertEqual((udir / "someone_elses_memory.md").read_text(),
                         '---\nname: someone_elses_memory\nscope: user\n---\nforeign body\n')
        self.assertEqual((udir / "another_foreign.md").read_text(), "foreign 2\n")
        # owned files: restored to the flat pool, gone from the user layer
        self.assertTrue((self.base / "memory" / "beta_user.md").exists())
        self.assertFalse((udir / "beta_user.md").exists())
        self.assertIn("foreign user files match the manifest", out)
        self.assertIn("removed 3 owned files from user layer", out)

    def test_forward_aborts_when_backup_manifest_missing(self):
        self.write_pool()
        backup = self.base / "backups" / M.BACKUP_DIRNAME
        backup.mkdir(parents=True)  # interrupted step 1: dir but no manifest
        rc, out, err = self.run_script()
        self.assertEqual(rc, 1)
        self.assertIn("manifest.json is missing", err)
        # nothing mutated
        self.assertFalse((self.base / USER_DIR_REL / "beta_user.md").exists())
        self.assertTrue((self.base / "memory" / "beta_user.md").exists())

    def test_rollback_aborts_when_backup_manifest_missing(self):
        self.write_pool()
        rc, _, _ = self.run_script()
        self.assertEqual(rc, 0)
        (self.base / "backups" / M.BACKUP_DIRNAME / "manifest.json").unlink()
        before = self.snapshot()
        rc, out, err = self.run_script("--rollback")
        self.assertEqual(rc, 1)
        self.assertIn("manifest.json is missing", err)
        self.assertEqual(self.snapshot(), before)  # nothing mutated

    def test_rerun_noop_with_foreign_files_in_user_layer(self):
        self.write_pool()
        udir = self.base / USER_DIR_REL
        udir.mkdir(parents=True)
        (udir / "someone_elses_memory.md").write_text("foreign body\n")
        foreign_before = (udir / "someone_elses_memory.md").read_text()
        rc, out, err = self.run_script()
        self.assertEqual(rc, 0, out + err)
        self.assertIn("someone_elses_memory", out)  # reported as untouched
        manifest = json.loads((self.base / "backups" / M.BACKUP_DIRNAME /
                               "manifest.json").read_text())
        self.assertEqual(manifest["foreign_user_files"], ["someone_elses_memory.md"])
        before = self.snapshot()
        rc, out, err = self.run_script()  # re-run must be a full no-op
        self.assertEqual(rc, 0, out + err)
        self.assertIn("no-op", out)
        self.assertEqual(self.snapshot(), before)
        self.assertEqual((udir / "someone_elses_memory.md").read_text(), foreign_before)

    def test_tsv_changed_after_backup_aborts(self):
        self.write_pool()
        rc, _, _ = self.run_script()
        self.assertEqual(rc, 0)
        tsv = self.base / "memory-migration" / "migration-final.tsv"
        tsv.write_text(tsv.read_text().replace("gamma_user\tuser", "gamma_user\tagent"))
        rc, out, err = self.run_script()
        self.assertEqual(rc, 1)
        self.assertIn("TSV changed after the backup", err)

    def test_foreign_baseline_change_aborts(self):
        self.write_pool()
        udir = self.base / USER_DIR_REL
        udir.mkdir(parents=True)
        (udir / "someone_elses_memory.md").write_text("foreign body\n")
        rc, _, _ = self.run_script()
        self.assertEqual(rc, 0)
        (udir / "someone_elses_memory.md").unlink()  # foreign file vanished
        rc, out, err = self.run_script()
        self.assertEqual(rc, 1)
        self.assertIn("foreign baseline", err)

    def test_pending_files_untouched(self):
        self.write_pool()
        pending = self.base / "memory" / "ghost.md.pending"
        pending.write_text("partial write\n")
        rc, out, err = self.run_script()
        self.assertEqual(rc, 0, out + err)
        self.assertEqual(pending.read_text(), "partial write\n")  # left exactly as-is
        self.assertFalse((self.base / USER_DIR_REL / "ghost.md.pending").exists())


if __name__ == "__main__":
    unittest.main()
