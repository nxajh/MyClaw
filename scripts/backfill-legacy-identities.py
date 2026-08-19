#!/usr/bin/env python3
"""回填历史 routing_key → user_id 关联（一次性数据修复）。

背景：web 客户端渠道（`client:default:...`）的身份生成方式在某次重构中收敛为
单一稳定值 `client:default:web-user:default`（一个浏览器/用户对应一个固定
routing_key）。在此之前，同一渠道下的历史身份是按连接 / 旧 client_id 生成的
（形如 `client:default:ws-1`、`client:default:ws-27`、
`client:default:web:<uuid>` 等），彼此互不相同。

`SessionManager::list_sessions_for_user`（G44）只按 `user_resolver.json` 里
的 override 聚合会话——旧连接身份从未被登记为 override，所以它们名下的会话
在新身份体系里"消失"了（数据都还在 `sessions/`，只是列表查不到）。

本脚本扫描 `{base}/sessions/*/meta.json` 收集所有旧 owner，把与 `--like`
指定的参考 routing_key 共享同一 `channel:account:` 前缀、且尚未登记 override
的 owner，全部指向参考 key 当前解析到的 user_id。

用法：
    python3 scripts/backfill-legacy-identities.py --base ~/.myclaw \\
        --like client:default:web-user:default --dry-run
    python3 scripts/backfill-legacy-identities.py --base ~/.myclaw \\
        --like client:default:web-user:default --apply

必须先停机（daemon 存活则拒绝执行）：UserResolver 只在启动时从磁盘加载一次，
且运行中每次 `/link` 等操作都会用内存态整体覆盖写回文件——若不停机，本脚本
写入的内容会被之后的自动保存悄悄覆盖掉。
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
import time
from pathlib import Path
from typing import Any

OWNER_RE = re.compile(r'"owner"\s*:\s*"([^"]*)"')


def check_daemon_stopped(base: Path) -> None:
    pid_file = base / "myclaw.pid"
    if pid_file.exists():
        raw = pid_file.read_text().strip()
        if raw.isdigit() and Path(f"/proc/{raw}").exists():
            sys.exit(f"错误：检测到 daemon 进程存活（pid {raw}）。请先 myclaw stop 再执行。")
    if os.path.isdir("/proc"):
        for pid in os.listdir("/proc"):
            if not pid.isdigit():
                continue
            try:
                cmd = Path(f"/proc/{pid}/cmdline").read_bytes().split(b"\0")
            except OSError:
                continue
            if any(b"myclaw" in c and b".py" not in c for c in cmd[:2] if c):
                sys.exit(f"错误：检测到疑似 myclaw 进程（pid {pid}）。请先 myclaw stop 再执行。")


def load_resolver(path: Path) -> dict[str, Any]:
    if not path.exists():
        sys.exit(f"错误：{path} 不存在")
    try:
        doc = json.loads(path.read_text())
    except json.JSONDecodeError as e:
        sys.exit(f"错误：{path} 不是合法 JSON：{e}")
    doc.setdefault("version", 1)
    doc.setdefault("overrides", {})
    return doc


def scan_owners(sessions_dir: Path) -> set[str]:
    """收集所有 session 的 owner 字段。跳过 `.legacy/`（旧布局归档，与
    当前身份体系无关）与 `.bak`（备份文件，非活跃数据）。"""
    owners: set[str] = set()
    if not sessions_dir.is_dir():
        return owners
    for meta in sessions_dir.glob("*/meta.json"):
        if ".legacy" in meta.parts or any(p.endswith(".bak") for p in meta.parts):
            continue
        try:
            text = meta.read_text()
        except OSError:
            continue
        m = OWNER_RE.search(text)
        if m:
            owners.add(m.group(1))
    return owners


def main() -> None:
    ap = argparse.ArgumentParser(description="回填历史渠道身份到当前 UserResolver override")
    ap.add_argument("--base", type=Path, required=True, help="base_dir（含 sessions/、user_resolver.json）")
    ap.add_argument(
        "--like",
        required=True,
        help="参考 routing_key：新旧身份共享的 channel:account 前缀取自这里，"
        "目标 user_id 也取这个 key 当前在 user_resolver.json 里解析到的值",
    )
    ap.add_argument("--dry-run", action="store_true")
    ap.add_argument("--apply", action="store_true")
    args = ap.parse_args()
    if args.dry_run == args.apply:
        ap.error("--dry-run 和 --apply 二选一")

    base = args.base.expanduser().resolve()
    resolver_path = base / "user_resolver.json"
    doc = load_resolver(resolver_path)
    overrides: dict[str, str] = doc["overrides"]

    target_uid = overrides.get(args.like)
    if not target_uid:
        sys.exit(
            f"错误：{args.like} 在 {resolver_path} 里没有 override 记录，"
            "不知道要折叠到哪个 user_id（先确认这个参考 key 本身已经关联好）"
        )

    parts = args.like.split(":")
    if len(parts) < 2:
        sys.exit(f"错误：--like 应为 channel:account:... 形式的 routing_key，收到 {args.like!r}")
    prefix = f"{parts[0]}:{parts[1]}:"

    owners = scan_owners(base / "sessions")
    candidates = sorted(
        o for o in owners if o.startswith(prefix) and o not in overrides
    )

    if not candidates:
        print(f"没有找到需要回填的历史身份（前缀 {prefix!r}，目标已覆盖所有已知 owner）。")
        return

    print(f"参考 key：{args.like} → {target_uid}")
    print(f"前缀：{prefix!r}，共找到 {len(candidates)} 个待回填的历史身份：")
    for o in candidates:
        print(f"  {o}  →  {target_uid}")

    if args.dry_run:
        print("\n（--dry-run，未写入。确认无误后加 --apply 执行。）")
        return

    check_daemon_stopped(base)

    backup_path = resolver_path.with_name(
        f"user_resolver.json.bak.{time.strftime('%Y%m%d%H%M%S')}"
    )
    backup_path.write_text(resolver_path.read_text())
    print(f"\n已备份原文件到 {backup_path}")

    for o in candidates:
        overrides[o] = target_uid
    resolver_path.write_text(json.dumps(doc, indent=2, ensure_ascii=False) + "\n")
    print(f"已写入 {len(candidates)} 条 override 到 {resolver_path}")
    print("请重启 daemon 使其重新加载 user_resolver.json。")


if __name__ == "__main__":
    main()
