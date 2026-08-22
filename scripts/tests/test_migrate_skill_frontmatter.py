#!/usr/bin/env python3
# migrate_skill_frontmatter.py fixture 自测：非标顶层字段收敛到 metadata；幂等；
# 已有 metadata 合并；已迁移文件跳过；无 frontmatter 文件跳过。issue #125。
# 运行：python3 -m unittest discover -s scripts/tests -v

import importlib.util
import sys
import tempfile
import unittest
from pathlib import Path

_SCRIPT = Path(__file__).resolve().parents[1] / "migrate_skill_frontmatter.py"
_spec = importlib.util.spec_from_file_location("migrate_skill_frontmatter", _SCRIPT)
migrate_skill_frontmatter = importlib.util.module_from_spec(_spec)
sys.modules["migrate_skill_frontmatter"] = migrate_skill_frontmatter
_spec.loader.exec_module(migrate_skill_frontmatter)
M = migrate_skill_frontmatter


class MigrateSkillFrontmatterTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.skills_dir = Path(self.tmp.name) / "skills"
        self.skills_dir.mkdir()

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def write_skill(self, folder: str, content: str) -> Path:
        d = self.skills_dir / folder
        d.mkdir(parents=True)
        p = d / "SKILL.md"
        p.write_text(content, encoding="utf-8")
        return p

    def test_moves_simple_top_level_field(self) -> None:
        p = self.write_skill(
            "weather",
            "---\nname: weather\ndescription: \"Get weather\"\n"
            "keywords: [weather, forecast]\n---\n\n# Weather\n",
        )
        changed = M.migrate_file(p)
        self.assertTrue(changed)
        out = p.read_text(encoding="utf-8")
        self.assertIn("metadata:\n  keywords: [weather, forecast]", out)
        self.assertNotIn("\nkeywords:", out.split("metadata:")[0])
        # Body preserved verbatim.
        self.assertIn("# Weather", out)

    def test_dry_run_does_not_write(self) -> None:
        p = self.write_skill(
            "weather",
            "---\nname: weather\ndescription: \"x\"\nkeywords: [a]\n---\nbody\n",
        )
        before = p.read_text(encoding="utf-8")
        changed = M.migrate_file(p, dry_run=True)
        self.assertTrue(changed)
        self.assertEqual(p.read_text(encoding="utf-8"), before)

    def test_idempotent_second_run_is_noop(self) -> None:
        p = self.write_skill(
            "flight",
            "---\nname: flight\ndescription: \"x\"\nversion: 1.0\n"
            "arguments:\n  - a\n  - b\nstatus: draft\n---\nbody\n",
        )
        first = M.migrate_file(p)
        self.assertTrue(first)
        migrated = p.read_text(encoding="utf-8")

        second = M.migrate_file(p)
        self.assertFalse(second)
        self.assertEqual(p.read_text(encoding="utf-8"), migrated)

    def test_merges_with_preexisting_metadata_block(self) -> None:
        p = self.write_skill(
            "flight",
            "---\nname: flight\ndescription: \"x\"\nversion: 1.0\n"
            "metadata:\n  extra: \"already here\"\n---\nbody\n",
        )
        M.migrate_file(p)
        out = p.read_text(encoding="utf-8")
        self.assertIn('extra: "already here"', out)
        self.assertIn('version: "1.0"', out)
        # Only one metadata: block, not two.
        self.assertEqual(out.count("metadata:"), 1)

    def test_top_level_wins_over_existing_metadata_value_for_same_key(self) -> None:
        p = self.write_skill(
            "dual",
            "---\nname: dual\ndescription: \"x\"\nversion: top-level\n"
            "metadata:\n  version: under-metadata\n---\nbody\n",
        )
        M.migrate_file(p)
        out = p.read_text(encoding="utf-8")
        self.assertIn('version: "top-level"', out)
        self.assertNotIn("under-metadata", out)

    def test_quotes_known_scalar_fields_but_not_lists_or_bools(self) -> None:
        p = self.write_skill(
            "flight",
            "---\nname: flight\ndescription: \"x\"\nversion: 1.0\n"
            "user_invocable: true\nkeywords: [a, b]\n---\nbody\n",
        )
        M.migrate_file(p)
        out = p.read_text(encoding="utf-8")
        self.assertIn('version: "1.0"', out)
        self.assertIn("user_invocable: true", out)
        self.assertNotIn('user_invocable: "true"', out)
        self.assertIn("keywords: [a, b]", out)

    def test_already_migrated_file_is_skipped(self) -> None:
        content = "---\nname: weather\ndescription: \"x\"\nmetadata:\n  keywords: [a]\n---\nbody\n"
        p = self.write_skill("weather", content)
        changed = M.migrate_file(p)
        self.assertFalse(changed)
        self.assertEqual(p.read_text(encoding="utf-8"), content)

    def test_name_and_description_only_is_noop(self) -> None:
        content = "---\nname: weather\ndescription: \"x\"\n---\nbody\n"
        p = self.write_skill("weather", content)
        changed = M.migrate_file(p)
        self.assertFalse(changed)

    def test_no_frontmatter_is_skipped(self) -> None:
        p = self.write_skill("plain", "# Just a heading\n\nNo frontmatter here.\n")
        changed = M.migrate_file(p)
        self.assertFalse(changed)

    def test_malformed_frontmatter_is_skipped(self) -> None:
        p = self.write_skill("broken", "---\nname: broken\nno closing marker\n")
        changed = M.migrate_file(p)
        self.assertFalse(changed)

    def test_preserves_body_verbatim_including_code_fences(self) -> None:
        body = "\n# Title\n\n```yaml\nkey: value\n```\n\nMore text.\n"
        p = self.write_skill(
            "flight", "---\nname: flight\ndescription: \"x\"\nversion: 1.0\n---" + body
        )
        M.migrate_file(p)
        out = p.read_text(encoding="utf-8")
        self.assertIn("```yaml\nkey: value\n```", out)
        self.assertIn("More text.", out)

    def test_multiple_files_scanned_by_main_glob(self) -> None:
        self.write_skill("a", "---\nname: a\ndescription: \"x\"\nkeywords: [a]\n---\nbody\n")
        self.write_skill("b", "---\nname: b\ndescription: \"x\"\n---\nbody\n")
        files = sorted(self.skills_dir.glob("*/SKILL.md"))
        self.assertEqual(len(files), 2)
        results = [M.migrate_file(f) for f in files]
        self.assertEqual(results, [True, False])


if __name__ == "__main__":
    unittest.main()
