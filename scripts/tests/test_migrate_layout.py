#!/usr/bin/env python3
# migrate-layout.py fixture 自测：构造旧布局样例 -> apply -> 断言新布局；幂等；rollback 往返。
# 运行：python3 -m unittest discover -s scripts/tests -v（或 python3 scripts/tests/test_migrate_layout.py）

import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path

_SCRIPT = Path(__file__).resolve().parents[1] / "migrate-layout.py"
_spec = importlib.util.spec_from_file_location("migrate_layout", _SCRIPT)
migrate_layout = importlib.util.module_from_spec(_spec)
sys.modules["migrate_layout"] = migrate_layout
_spec.loader.exec_module(migrate_layout)
M = migrate_layout


class MigrateLayoutTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        base = Path(self.tmp.name)
        self.ws = base / "ws"
        self.data = base / "data"
        self.ws.mkdir()
        self.data.mkdir()
        # 测试环境屏蔽真实 daemon 自检（/proc 会扫到本机在跑的 myclaw）
        M.check_daemon_stopped = lambda data: None
        # data 侧预置：已有 memory（1 个无 scope 的 agent md）与 state
        (self.data / "memory").mkdir()
        (self.data / "memory" / "preexisting.md").write_text("---\nname: p\n---\nbody\n")
        (self.data / "state").mkdir()
        (self.data / "state" / "wechat_buf.json").write_text("{}")

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def build_old_layout(self) -> None:
        ws = self.ws
        # sessions：2 个 myclaw_s_ + delegations 1 + 旧 routing-key 1 + .bak 1 + active.json(.bak)
        (ws / "sessions").mkdir()
        for u in ("019fe342-6a03-7561-86de-0c2327a8c3de",
                  "019ffe90-a1bd-7800-ac03-c749190e7827"):
            sd = ws / "sessions" / f"myclaw_s_{u}"
            sd.mkdir(parents=True)
            (sd / "meta.json").write_text(json.dumps(
                {"id": f"myclaw/s/{u}", "created_at": "2026-08-01"}))
            (sd / "history.jsonl").write_text('{"role":"user"}\n')
        (ws / "sessions" / "delegations").mkdir()
        (ws / "sessions" / "delegations" /
         "myclaw_s_019ffe90-a1bd-7800-ac03-c749190e7827.json").write_text('{"sub_session_id": 1}')
        (ws / "sessions" / "telegram:myclaw:6270938644").mkdir()
        (ws / "sessions" / "9ac06271.bak").mkdir()
        (ws / "sessions" / "active.json").write_text(json.dumps(
            {"main:myclaw/s/019fe342-6a03-7561-86de-0c2327a8c3de":
             "myclaw/s/019fe342-6a03-7561-86de-0c2327a8c3de"}))
        (ws / "sessions" / "active.json.bak").write_text("{}")
        # users：主用户 memory 2 md + 双前缀 1 md + root 1 md + legacy-rk-archive
        um = ws / "users" / "myclaw" / "u" / "019fe342-6a03-7561-86de-0c2327a8c3de" / "memory"
        um.mkdir(parents=True)
        (um / "user-note.md").write_text("---\nname: un\ndescription: d\n---\n\nbody\n")
        (um / "user-note2.md").write_text("no frontmatter body\n")
        dbl = (ws / "users" / "myclaw" / "u" / "myclaw" / "u" /
               "019fe342-6a03-7561-86de-0c2327a8c3de" / "memory")
        dbl.mkdir(parents=True)
        (dbl / "dbl-note.md").write_text("---\nname: dbl\n---\n\nbody\n")
        rt = ws / "users" / "myclaw" / "u" / "root" / "memory"
        rt.mkdir(parents=True)
        (rt / "root-note.md").write_text("root legacy\n")
        (ws / "users" / ".legacy-rk-archive" / "telegram:x").mkdir(parents=True)
        # memory：agent 层 2 md（无 scope）
        (ws / "memory").mkdir()
        (ws / "memory" / "agent-one.md").write_text("---\nname: a1\ntags: [t]\n---\n\nbody\n")
        (ws / "memory" / "agent-two.md").write_text("bare agent memory\n")
        # cron：jobs.json 1 条 + run_logs 1 个 + 用户笔记 + 旧 bak（后两者留在 workspace）
        (ws / "cron").mkdir()
        jid = "myclaw/job/019fe4ce-9e19-7da1-9235-7bc312adb456"
        (ws / "cron" / "jobs.json").write_text(json.dumps({"jobs": [
            {"id": jid, "schedule": "0 0 10 * * 5", "name": "ruanyifeng-weekly"}]}))
        (ws / "cron" / "run_logs").mkdir()
        (ws / "cron" / "run_logs" /
         "myclaw_job_019fe4ce-9e19-7da1-9235-7bc312adb456.jsonl").write_text("{}\n")
        (ws / "cron" / "ruanyifeng-weekly.md").write_text("user notes stay\n")
        (ws / "cron" / "jobs.json.bak").write_text("{}")
        # agents/skills/backups/.state/归档批次
        (ws / "agents" / "coder").mkdir(parents=True)
        (ws / "skills" / "demo").mkdir(parents=True)
        (ws / "backups").mkdir()
        (ws / "backups" / "old.tar.gz").write_text("")
        (ws / ".state").mkdir()
        (ws / ".state" / "tasks.json").write_text("{}")
        (ws / ".state" / "completion_queue").mkdir()
        (ws / ".state" / "completion_queue" / "a.json").write_text("{}")
        (ws / ".state" / "inbound_spool").mkdir()
        (ws / "sessions.zombie-archive-20260701").mkdir()

    def test_full_apply_idempotent_and_rollback(self) -> None:
        self.build_old_layout()
        ws, d = self.ws, self.data
        M.apply(ws, d)

        # sessions
        u1 = d / "sessions" / "019fe342-6a03-7561-86de-0c2327a8c3de"
        self.assertTrue((u1 / "meta.json").exists())
        self.assertFalse((d / "sessions" / "myclaw_s_019fe342-6a03-7561-86de-0c2327a8c3de").exists())
        self.assertFalse((d / "sessions" / "delegations").exists())
        self.assertTrue(
            (d / "sessions" / "019ffe90-a1bd-7800-ac03-c749190e7827" / "delegation.json").exists())
        self.assertTrue((d / "sessions" / ".legacy" / "telegram:myclaw:6270938644").is_dir())
        self.assertTrue((d / "sessions" / ".legacy" / "active.json.bak").exists())
        self.assertTrue((d / "sessions" / ".legacy" / "tasks.json").exists())
        self.assertTrue((d / "sessions" / ".legacy" / "sessions.zombie-archive-20260701").is_dir())
        self.assertTrue((d / "sessions" / "active.json").exists())
        # jobs
        jd = d / "jobs" / "019fe4ce-9e19-7da1-9235-7bc312adb456"
        self.assertTrue((jd / "meta.json").exists())
        self.assertEqual(json.loads((jd / "meta.json").read_text())["name"], "ruanyifeng-weekly")
        self.assertTrue((jd / "history.jsonl").exists())
        self.assertTrue((d / "backups" / "pre-layout" / "jobs.json.bak").exists())
        self.assertTrue((ws / "cron" / "ruanyifeng-weekly.md").exists())
        self.assertFalse((ws / "cron" / "jobs.json").exists())
        # memory 平铺 + scope
        un = (d / "memory" / "user-note.md").read_text()
        self.assertIn('scope: "user"', un)
        self.assertIn('user_id: "myclaw/u/019fe342-6a03-7561-86de-0c2327a8c3de"', un)
        self.assertIn('scope: "user"', (d / "memory" / "dbl-note.md").read_text())
        self.assertIn('scope: "agent"', (d / "memory" / "agent-one.md").read_text())
        self.assertIn('scope: "agent"', (d / "memory" / "agent-two.md").read_text())
        self.assertIn('scope: "agent"', (d / "memory" / "preexisting.md").read_text())
        self.assertTrue((d / "users" / ".legacy-root-memory" / "root-note.md").exists())
        # users 清壳、legacy-rk-archive 保留
        self.assertFalse((d / "users" / "myclaw").exists())
        self.assertTrue((d / "users" / ".legacy-rk-archive").is_dir())
        # agents/skills/backups/.state
        self.assertTrue((d / "agents" / "coder").is_dir())
        self.assertTrue((d / "skills" / "demo").is_dir())
        self.assertTrue((d / "backups" / "old.tar.gz").exists())
        self.assertTrue((d / "state" / "completion_queue" / "a.json").exists())
        self.assertTrue((d / "state" / "inbound_spool").is_dir())
        self.assertTrue((d / "state" / "wechat_buf.json").exists())
        self.assertFalse((ws / ".state").exists())
        for absent in ("sessions", "users", "memory", "agents", "skills", "backups"):
            self.assertFalse((ws / absent).exists(), absent)
        # verify 通过
        self.assertEqual(M.verify(ws, d), 0)
        # 幂等：重跑计划为空、apply 不报错
        self.assertFalse(M.build_plan(ws, d).actions)
        M.apply(ws, d)
        # rollback 往返：旧布局关键位置恢复
        M.rollback(ws, d)
        self.assertTrue(
            (ws / "sessions" / "myclaw_s_019fe342-6a03-7561-86de-0c2327a8c3de" / "meta.json").exists())
        self.assertTrue((ws / "sessions" / "delegations" /
                         "myclaw_s_019ffe90-a1bd-7800-ac03-c749190e7827.json").exists())
        self.assertTrue((ws / "users" / "myclaw" / "u" / "019fe342-6a03-7561-86de-0c2327a8c3de" /
                         "memory" / "user-note.md").exists())
        self.assertNotIn("scope", (ws / "memory" / "agent-one.md").read_text())
        self.assertTrue((ws / "cron" / "jobs.json").exists())
        self.assertTrue((ws / "backups" / "old.tar.gz").exists())
        self.assertFalse((d / "jobs" / "019fe4ce-9e19-7da1-9235-7bc312adb456" / "meta.json").exists())

    def test_memory_name_conflict_fails_fast(self) -> None:
        self.build_old_layout()
        (self.data / "memory" / "agent-one.md").write_text("---\n---\n")
        with self.assertRaises(SystemExit):
            M.dry_run(self.ws, self.data)

    def test_inject_frontmatter(self) -> None:
        f = migrate_layout.inject_frontmatter
        self.assertIsNone(f('---\nscope: "agent"\n---\n', "agent"))
        out = f('---\nname: x\n---\nbody', "user", "myclaw/u/abc")
        self.assertIn('scope: "user"', out)
        self.assertIn('user_id: "myclaw/u/abc"', out)
        self.assertLess(out.index('scope:'), out.index('---\nbody'))
        bare = f("just body", "agent")
        self.assertTrue(bare.startswith('---\nscope: "agent"\n---\n\njust body'))


if __name__ == "__main__":
    unittest.main()
