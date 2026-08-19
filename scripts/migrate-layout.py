#!/usr/bin/env python3
"""MyClaw 存储布局迁移脚本（P1）。设计文档：docs/storage-layout-and-trigger-redesign.md §5。

旧布局（实体数据在 workspace 下）→ 新布局（sessions/users/jobs/memory 等归 base dir，
session 目录裸 uuid、users/jobs 拆分目录化、memory 平铺 + frontmatter scope 属性）。

用法：
    python3 scripts/migrate-layout.py --dry-run    # 审查动作清单（零写入）
    python3 scripts/migrate-layout.py --apply      # 停机执行（daemon 存活则拒绝）
    python3 scripts/migrate-layout.py --verify     # 迁移后对账
    python3 scripts/migrate-layout.py --rollback   # 按 manifest 反向回滚

语义：fail-fast（单步失败立即中止并报告；重跑安全——已执行步骤自动跳过）。
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import sys
import tarfile
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Optional

UUID_RE = re.compile(r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$")
UUID_ANY_RE = re.compile(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")
SESSION_PREFIX = "myclaw_s_"
NAMESPACE = "myclaw"


def rel(p: Path, base: Path) -> str:
    try:
        return str(p.relative_to(base))
    except ValueError:
        return str(p)


@dataclass
class Action:
    kind: str  # move | created | modified | removed_dir | notify
    note: str
    src: Optional[Path] = None
    dst: Optional[Path] = None
    meta: dict[str, Any] = field(default_factory=dict)


@dataclass
class Plan:
    actions: list[Action] = field(default_factory=list)

    def add(self, **kw: Any) -> None:
        self.actions.append(Action(**kw))


# ── frontmatter 注入 ─────────────────────────────────────────────────────────


def inject_frontmatter(
    text: str, scope: str, user_id: str | None = None, mem_type: str | None = None
) -> str | None:
    """注入 scope（+user_id、+type）到 frontmatter；所有待注入的 key 均已存在则返回
    None（幂等）。`type` 是 Rust 侧 `MemoryFile` 的必填字段（缺失直接解析失败、
    对整个系统不可见——不只是分类丢失），遗留 type 分区目录（如 project/、
    reference/）拍平时必须一起补上，不能只补 scope。"""
    lines = text.splitlines(keepends=True)
    has_fm = bool(lines) and lines[0].strip() == "---"
    keys: list[str] = [f'scope: "{scope}"']
    if user_id:
        keys.append(f'user_id: "{user_id}"')
    if mem_type:
        keys.append(f'type: "{mem_type}"')

    def find_key(fm_lines: list[str], key: str) -> bool:
        return any(ln.startswith(f"{key}:") for ln in fm_lines)

    if has_fm:
        end = None
        for i in range(1, len(lines)):
            if lines[i].strip() == "---":
                end = i
                break
        if end is None:
            raise ValueError("frontmatter 未闭合（缺第二个 '---'）")
        fm = lines[1:end]
        inject = [k + "\n" for k in keys if not find_key(fm, k.split(":")[0])]
        if not inject:
            return None
        return "".join(lines[:end]) + "".join(inject) + "".join(lines[end:])
    return "---\n" + "".join(k + "\n" for k in keys) + "---\n\n" + text


# ── 前置自检 ────────────────────────────────────────────────────────────────


def check_daemon_stopped(base: Path) -> None:
    pid_file = base / "myclaw.pid"
    if pid_file.exists():
        raw = pid_file.read_text().strip()
        if raw.isdigit() and Path(f"/proc/{raw}").exists():
            sys.exit(f"错误：检测到 daemon 进程存活（pid {raw}）。请先 myclaw stop 再迁移。")
    if os.path.isdir("/proc"):
        for pid in os.listdir("/proc"):
            if not pid.isdigit():
                continue
            try:
                cmd = Path(f"/proc/{pid}/cmdline").read_bytes().split(b"\0")
            except OSError:
                continue
            if any(b"myclaw" in c and b".py" not in c for c in cmd[:2] if c):
                sys.exit(f"错误：检测到疑似 myclaw 进程（pid {pid}）。请先 myclaw stop 再迁移。")


# ── memory 子目录归位（.versions / .audit / 遗留 type 分区）──────────────────
#
# B12/B13 原本只 glob("*.md") 扫 memory 目录的第一层，完全没处理三类真实存在
# 的子目录：
#   .versions/{name}/v{N}__{date}__{hash}.md —— memory_tool.rs 的版本归档，
#       目标同样是扁平池下的 .versions/{name}/（不区分用户，与运行时布局一致）
#   .audit/ —— 运行时审计日志，目标是 {base}/state/memory/.audit/（不在
#       memory 池里，memory_tool.rs 写在 base_dir/state/memory/.audit）
#   其它任意子目录（如 project/、reference/）—— 更早期"按 type 分子目录"的
#       遗留布局，拍平进 memory 池并补 type: <目录名>（MemoryFile.mem_type 是
#       必填字段，缺失直接解析失败、对整个系统不可见，不只是分类丢失）
def migrate_memory_subdir_extras(
    p: Plan, memdir: Path, B: Any, scope: str, user_id: str | None, note_prefix: str
) -> None:
    if not memdir.is_dir():
        return
    versions_dir = memdir / ".versions"
    if versions_dir.is_dir():
        for f in sorted(versions_dir.rglob("*.md")):
            rel_path = f.relative_to(versions_dir)
            dst = B("memory", ".versions", *rel_path.parts)
            if dst.exists():
                if dst.read_bytes() == f.read_bytes():
                    continue  # 内容一致（哈希落在文件名里）——之前已迁移过，幂等跳过
                sys.exit(f"错误：memory 版本归档冲突 {rel_path}（同名不同内容）——人工决断")
            p.add(kind="move", src=f, dst=dst, note=f"{note_prefix} .versions 归位")
    audit_dir = memdir / ".audit"
    if audit_dir.is_dir():
        for f in sorted(audit_dir.rglob("*")):
            if f.is_dir():
                continue
            rel_path = f.relative_to(audit_dir)
            dst = B("state", "memory", ".audit", *rel_path.parts)
            if dst.exists():
                print(f"提示：{rel_path} 目标已存在，保留旧文件，跳过迁移源 {f}")
                continue
            p.add(kind="move", src=f, dst=dst, note=f"{note_prefix} .audit 归位")
    for sub in sorted(memdir.iterdir()):
        if not sub.is_dir() or sub.name in (".versions", ".audit"):
            continue
        mem_type = sub.name
        for f in sorted(sub.rglob("*.md")):
            dst = B("memory", f.name)
            if dst.exists():
                print(f"警告：遗留 type={mem_type} 分区 memory 同名跳过 {f.name}")
                continue
            p.add(kind="move", src=f, dst=dst, note=f"{note_prefix} 遗留 type={mem_type} 拍平",
                  meta={"scope": scope, "user_id": user_id, "mem_type": mem_type})


# ── 计划构建 ────────────────────────────────────────────────────────────────


def build_plan(ws: Path, base: Path) -> Plan:
    p = Plan()

    def W(*parts: str) -> Path:
        return ws.joinpath(*parts)

    def B(*parts: str) -> Path:
        return base.joinpath(*parts)

    # ── A 组：目录搬迁（workspace → base dir） ──
    # A1 backups 先搬（tar 备份要落在 base/backups/pre-layout/ 里）
    if W("backups").exists() and not B("backups").exists():
        p.add(kind="move", src=W("backups"), dst=B("backups"), note="A1 backups/ → base")
    if W("sessions").exists() and not B("sessions").exists():
        p.add(kind="move", src=W("sessions"), dst=B("sessions"), note="A2 sessions/ → base")
    if W("users").exists() and not B("users").exists():
        p.add(kind="move", src=W("users"), dst=B("users"), note="A3 users/ → base")
    # A4 memory：逐 md 合并进 base/memory（base 侧可能已有少量文件）。子目录
    # （.versions/.audit/遗留 type 分区，如 project/、reference/）单独归位——
    # 历史上这里只扫第一层 *.md，子目录内容会被无声漏掉。
    if W("memory").is_dir():
        for f in sorted(W("memory").glob("*.md")):
            dst = B("memory", f.name)
            if dst.exists():
                sys.exit(f"错误：memory 名称冲突 {f.name}（base/memory 已存在同名文件）——人工决断后重跑")
            p.add(kind="move", src=f, dst=dst, note="A4 memory 平铺合并")
        migrate_memory_subdir_extras(p, W("memory"), B, "agent", None, "A4")
    if W("memory").is_dir():
        # rmdir_empty_tree 自底向上清空壳，含文件移出后留下的空子目录
        # （.versions/xxx/、.audit/、遗留 type 分区）；真有文件剩下会保留+警告，
        # 不需要在这里预判"是不是已经空了"。
        p.add(kind="removed_dir", src=W("memory"), note="A4 memory 清壳")
    # A5 cron → jobs：jobs.json 拆分（created）+ run_logs → history.jsonl（move）
    jobs_json = W("cron", "jobs.json")
    if jobs_json.exists():
        entries = json.loads(jobs_json.read_text())["jobs"]
        for e in entries:
            jid = e.get("id", "")
            m = UUID_ANY_RE.search(jid)
            if not m:
                sys.exit(f"错误：jobs.json 条目无 FQID uuid：{jid!r}")
            p.add(kind="created", dst=B("jobs", m.group(0), "meta.json"),
                  note="A5 jobs.json 拆分", meta={"entry": e})
        rl = W("cron", "run_logs")
        if rl.is_dir():
            for f in sorted(rl.iterdir()):
                m = UUID_ANY_RE.search(f.name)
                if m:
                    p.add(kind="move", src=f, dst=B("jobs", m.group(0), "history.jsonl"),
                          note="A5 run_logs → history.jsonl")
                else:
                    print(f"警告：run_logs 无法解析 uuid，跳过 {f.name}")
        p.add(kind="move", src=jobs_json,
              dst=B("backups", "pre-layout", "jobs.json.bak"), note="A5 旧 jobs.json 归备份")
    # A6 agents / skills
    for name in ("agents", "skills"):
        if W(name).exists() and not B(name).exists():
            p.add(kind="move", src=W(name), dst=B(name), note=f"A6 {name}/ → base")
    # A7 旧 sessions 归档批次目录
    for d in sorted(ws.glob("sessions.*-archive*")):
        if d.is_dir():
            p.add(kind="move", src=d, dst=B("sessions", ".legacy", d.name),
                  note="A7 归档批次 → .legacy")
    # A8 .state：tasks.json → sessions/.legacy；其余 → base/state
    #
    # 目标已存在（常见场景：新代码已经在 base/state 直接跑起来一段时间，
    # workspace/.state 那份是重构前的遗留数据）不再整体报错退出——逐文件
    # 合并，已存在的目标条目原样跳过（execute_action 的 move 本来就是这个
    # 语义，这里只是让目录级冲突也享受同样的幂等 skip，而不是卡住整个迁移）。
    # 目录搬完后 removed_dir 会自底向上清理空壳；若真有文件级冲突，会保留
    # 该文件、打印警告，交给人工确认，而不是静默覆盖。
    if W(".state").is_dir():
        for f in sorted(W(".state").iterdir()):
            if f.name.startswith("tasks.json"):
                p.add(kind="move", src=f, dst=B("sessions", ".legacy", f.name),
                      note="A8 全局任务板归档（P1 起任务板 per-session）")
            elif f.is_dir():
                # 先保证目标目录本身存在——目录可能是空的（没有文件触发
                # mkdir(parents=True)），但仍需要在 base 侧占位存在。
                p.add(kind="mkdir", dst=B("state", f.name), note="A8 目录合并占位")
                for child in sorted(f.rglob("*")):
                    if child.is_dir():
                        continue
                    p.add(kind="move", src=child, dst=B("state", f.name, str(child.relative_to(f))),
                          note="A8 .state 运行时状态 → base/state（目录合并）")
                p.add(kind="removed_dir", src=f, note="A8 .state 子目录清壳")
            else:
                p.add(kind="move", src=f, dst=B("state", f.name),
                      note="A8 .state 运行时状态 → base/state")
        p.add(kind="removed_dir", src=W(".state"), note="A8 .state 清壳")

    # ── B 组：实体形态迁移（base dir 内） ──
    # 扫描位置带回退：A 组未执行时（dry-run / round1）实体仍在 ws 侧，扫等价位置
    def pick(name: str) -> Path:
        d, w = B(name), W(name)
        return d if d.is_dir() else (w if w.is_dir() else d)

    # B 组动作的 dst 一律指向 base 侧（src 用扫描侧：dry-run/round1 回退到 ws，
    # 执行时 round2 重建为 base 侧）
    d_sess = B("sessions")

    # B9 session 目录裸化
    sess = pick("sessions")
    if sess.is_dir():
        for d in sorted(sess.iterdir()):
            if not d.is_dir() or not d.name.startswith(SESSION_PREFIX):
                continue
            uuid = d.name[len(SESSION_PREFIX):]
            if not UUID_RE.match(uuid):
                print(f"警告：非 uuid 目录名跳过 {d.name}")
                continue
            p.add(kind="move", src=d, dst=d_sess / uuid, note="B9 session 目录裸化")
    # B10 delegations → session 目录内
    dele = sess / "delegations"
    if dele.is_dir():
        for f in sorted(dele.glob("*.json")):
            uuid = f.stem[len(SESSION_PREFIX):] if f.stem.startswith(SESSION_PREFIX) else ""
            if not UUID_RE.match(uuid):
                print(f"警告：delegation 文件名无法解析 uuid，跳过 {f.name}")
                continue
            p.add(kind="move", src=f, dst=d_sess / uuid / "delegation.json",
                  note="B10 checkpoint 归位 session")
        p.add(kind="removed_dir", src=dele, note="B10 delegations 清壳")
    # B11 旧 routing-key 目录 / *.bak → .legacy
    if sess.is_dir():
        for d in sorted(sess.iterdir()):
            if not d.is_dir():
                continue
            if d.name.startswith(("telegram:", "qqbot:")) or d.name.endswith(".bak"):
                p.add(kind="move", src=d, dst=d_sess / ".legacy" / d.name,
                      note="B11 旧格式目录归档")
        ab = sess / "active.json.bak"
        if ab.exists():
            p.add(kind="move", src=ab, dst=d_sess / ".legacy" / "active.json.bak",
                  note="B11 active.json.bak 归档")
    # B12 用户 memory 抽取平铺（双前缀并入主用户；root 归档不注入）
    users = pick("users")
    if users.is_dir():
        main = users / NAMESPACE / "u"
        if main.is_dir():
            for udir in sorted(main.iterdir()):
                if udir.is_dir() and UUID_RE.match(udir.name):
                    memdir = udir / "memory"
                    if not memdir.is_dir():
                        continue
                    user_id = f"{NAMESPACE}/u/{udir.name}"
                    for f in sorted(memdir.glob("*.md")):
                        dst = B("memory", f.name)
                        if dst.exists():
                            sys.exit(f"错误：memory 名称冲突 {f.name}（user 层迁移目标已存在）——人工决断")
                        p.add(kind="move", src=f, dst=dst, note="B12 user memory 平铺",
                              meta={"scope": "user", "user_id": user_id})
                    migrate_memory_subdir_extras(p, memdir, B, "user", user_id, "B12")
            dbl = main / NAMESPACE / "u"
            if dbl.is_dir():
                for udir in sorted(dbl.iterdir()):
                    if udir.is_dir() and UUID_RE.match(udir.name):
                        memdir = udir / "memory"
                        if not memdir.is_dir():
                            continue
                        user_id = f"{NAMESPACE}/u/{udir.name}"
                        for f in sorted(memdir.glob("*.md")):
                            dst = B("memory", f.name)
                            if dst.exists():
                                print(f"警告：双前缀 memory 同名跳过 {f.name}")
                                continue
                            p.add(kind="move", src=f, dst=dst, note="B12 双前缀并入主用户",
                                  meta={"scope": "user", "user_id": user_id})
                        migrate_memory_subdir_extras(p, memdir, B, "user", user_id, "B12 双前缀")
            root_mem = main / "root" / "memory"
            if root_mem.is_dir():
                # root 不注入共享池——.versions/遗留 type 分区也一并按原相对
                # 路径归档到 .legacy-root-memory，不补 scope/type（原样保留）。
                for f in sorted(root_mem.rglob("*.md")):
                    rel_path = f.relative_to(root_mem)
                    p.add(kind="move", src=f, dst=B("users", ".legacy-root-memory", *rel_path.parts),
                          note="B12 root memory 归档（不注入）")
        if (users / NAMESPACE).exists():
            p.add(kind="removed_dir", src=users / NAMESPACE,
                  note="B12 users/myclaw 清壳（.legacy-rk-archive 保留）")
    # B13 agent 层 memory 补 scope: agent（凡无 scope 键的；A4 已经把
    # workspace/memory 的内容——含子目录——并入 base/memory，这里只需要对
    # 落位后仍缺 scope 键的文件补齐，不用再 pick() 二选一）。
    mem_dir = B("memory")
    if mem_dir.is_dir():
        for f in sorted(mem_dir.glob("*.md")):
            txt = f.read_text()
            if inject_frontmatter(txt, "agent") is not None:
                p.add(kind="modified", src=f, note="B13 agent memory 补 scope",
                      meta={"scope": "agent"})
    # B14 users.json（若存在）拆分为 users/{uuid}/meta.json
    uj = B("users.json")
    if uj.exists():
        reg = json.loads(uj.read_text())
        entries = reg.get("users", reg) if isinstance(reg, dict) else {}
        for uid, user in entries.items():
            m = UUID_ANY_RE.search(uid or "")
            if not m:
                print(f"警告：users.json 条目无 uuid，跳过 {uid!r}")
                continue
            body = dict(user) if isinstance(user, dict) else {"value": user}
            body.setdefault("id", uid)
            p.add(kind="created", dst=B("users", m.group(0), "meta.json"),
                  note="B14 users.json 拆分", meta={"entry": body})
        p.add(kind="move", src=uj, dst=B("backups", "pre-layout", "users.json.bak"),
              note="B14 旧 users.json 归备份")
    # B15 heartbeat 提示（不修改 TOML）
    toml = base / "myclaw.toml"
    if toml.exists() and "[scheduler.heartbeat]" in toml.read_text():
        p.add(kind="notify",
              note="B15 检测到 [scheduler.heartbeat] 配置：P3 将删除该机制，请手动移除该配置段")

    return p


# ── 执行 ────────────────────────────────────────────────────────────────────


def make_backup(ws: Path, base: Path, plan: Plan, bak_dir: Path) -> Path:
    """备份被 in-place 修改/拆分的小文件（rename 类不备份，靠 manifest 逆向）。"""
    bundle = bak_dir / "bundle.tar.gz"
    if bundle.exists():
        print(f"备份已存在，保留首轮：{bundle}")
        return bundle
    bak_dir.mkdir(parents=True, exist_ok=True)
    targets: list[tuple[Path, str]] = []
    for a in plan.actions:
        if a.kind == "modified" and a.src:
            if a.src.is_relative_to(base):
                targets.append((a.src, f"base/{rel(a.src, base)}"))
            else:
                targets.append((a.src, f"ws/{rel(a.src, ws)}"))
    # round2 时实体已在 base 侧原位
    extra = [base / "sessions" / "active.json", base / "sessions" / "delegations"]
    for e in extra:
        if e.is_file():
            targets.append((e, f"base/{rel(e, base)}"))
        elif e.is_dir():
            for f in e.rglob("*.json"):
                targets.append((f, f"base/{rel(f, base)}"))
    with tarfile.open(bundle, "x:gz") as tar:
        for p, arc in targets:
            if p.exists():
                tar.add(p, arcname=arc)
    return bundle


def apply(ws: Path, base: Path) -> None:
    check_daemon_stopped(base)
    bak_dir = base / "backups" / "pre-layout"
    manifest: dict[str, Any] = {
        "created_at": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
        "workspace": str(ws), "base": str(base),
        "moves": [], "created": [], "modified": [], "removed_dirs": [], "notifies": [],
        "backup": str(bak_dir / "bundle.tar.gz"),
    }
    total = 0
    try:
        # 两轮构建：round1 搬 A 组（workspace → base）；round2 时 B 组扫描才能看到
        # 已落位的实体（B9 扫 base/sessions、B12 扫 base/users、B13 扫 base/memory）。
        for rnd in (1, 2):
            plan = build_plan(ws, base)
            if not plan.actions:
                break
            if rnd == 2:
                # B 组执行前打 tar：modified 源文件此刻均在 base 侧原位
                bundle = make_backup(ws, base, plan, bak_dir)
                print(f"备份完成：{bundle}")
            for a in plan.actions:
                try:
                    n = execute_action(ws, base, a, manifest)
                except Exception as e:  # noqa: BLE001 —— fail-fast：报告后中止，带上具体动作方便定位
                    sys.exit(
                        f"迁移中止（已完成 {total} 步）：{a.kind} "
                        f"src={a.src} dst={a.dst}（{a.note}）失败：{e}\n"
                        f"修复问题后直接重跑 --apply（已执行步骤自动跳过）。"
                    )
                total += n
    except Exception as e:  # noqa: BLE001 —— build_plan/make_backup 阶段的失败
        sys.exit(f"迁移中止（已完成 {total} 步）：{e}\n"
                 f"修复问题后直接重跑 --apply（已执行步骤自动跳过）。")

    if total == 0 and not manifest["notifies"]:
        print("无需迁移（数据已符合目标布局）。")
        return
    bak_dir.mkdir(parents=True, exist_ok=True)
    mf = bak_dir / "manifest.json"
    mf.write_text(json.dumps(manifest, ensure_ascii=False, indent=2))
    print(f"\n完成 {total} 步。manifest：{mf}")
    print("下一步：python3 scripts/migrate-layout.py --verify")


def rmdir_empty_tree(path: Path) -> bool:
    """自底向上删除纯空目录树；含任何文件则不动，返回是否删除。"""
    if not path.is_dir():
        return False
    for child in sorted(path.iterdir(), reverse=True):
        if child.is_dir():
            rmdir_empty_tree(child)
    if any(p.is_file() for p in path.rglob("*")):
        return False
    shutil.rmtree(path)
    return True


def execute_action(ws: Path, base: Path, a: Action, manifest: dict[str, Any]) -> int:
    """执行单个动作；返回 1=执行 / 0=跳过。"""
    if a.kind == "move":
        dst = a.dst
        if dst.exists():
            print(f"跳过（目标已存在）：{a.note} {dst}")
            return 0
        if not a.src.exists():
            # round1 构建的 B 组动作：A 组搬移后 src 已变（base 侧），round2 重建执行
            print(f"跳过（源已移动，待下轮）：{a.note} {a.src}")
            return 0
        dst.parent.mkdir(parents=True, exist_ok=True)
        shutil.move(str(a.src), str(dst))
        manifest["moves"].append({"from": str(a.src), "to": str(dst)})
        print(f"move {a.src} → {dst}")
        # A4/B12/B13 语义：move 携带 scope meta 时，落位后注入 frontmatter
        if "scope" in a.meta:
            txt = dst.read_text()
            new = inject_frontmatter(
                txt, a.meta["scope"], a.meta.get("user_id"), a.meta.get("mem_type")
            )
            if new is not None:
                dst.write_text(new)
                manifest["modified"].append(str(dst))
        return 1
    if a.kind == "created":
        if a.dst.exists():
            print(f"跳过（目标已存在）：{a.note} {a.dst}")
            return 0
        a.dst.parent.mkdir(parents=True, exist_ok=True)
        a.dst.write_text(json.dumps(a.meta["entry"], ensure_ascii=False, indent=2) + "\n")
        manifest["created"].append(str(a.dst))
        print(f"create {rel(a.dst, base)}")
        return 1
    if a.kind == "modified":
        if not a.src.exists():
            print(f"跳过（源已移动，待下轮）：{a.note} {a.src}")
            return 0
        txt = a.src.read_text()
        new = inject_frontmatter(txt, a.meta["scope"], a.meta.get("user_id"))
        if new is None:
            print(f"跳过（scope 已存在）：{a.note} {a.src.name}")
            return 0
        a.src.write_text(new)
        manifest["modified"].append(str(a.src))
        print(f"modify {a.src.name} (+scope={a.meta['scope']})")
        return 1
    if a.kind == "mkdir":
        if a.dst.exists():
            return 0
        a.dst.mkdir(parents=True, exist_ok=True)
        print(f"mkdir {rel(a.dst, base)}")
        return 1
    if a.kind == "removed_dir":
        if a.src.exists() and rmdir_empty_tree(a.src):
            manifest["removed_dirs"].append(str(a.src))
            print(f"rmtree(空壳) {a.src}")
            return 1
        if a.src.exists():
            print(f"警告：{a.src} 非空，保留待人工处理")
        return 0
    if a.kind == "notify":
        manifest["notifies"].append(a.note)
        print(f"[notify] {a.note}")
        return 1
    raise ValueError(f"未知动作类型 {a.kind}")


def find_manifest(base: Path) -> Path:
    backups = base / "backups"
    cands = sorted(backups.glob("pre-layout*/manifest.json")) if backups.is_dir() else []
    if not cands:
        sys.exit(f"错误：未找到迁移 manifest（{backups}/pre-layout*/manifest.json）")
    return cands[-1]


# ── 对账 / 回滚 ─────────────────────────────────────────────────────────────


def _has_scope(f: Path) -> bool:
    return re.search(r"^scope:", f.read_text(), re.M) is not None


def _scope_is(f: Path, val: str) -> bool:
    m = re.search(r'^scope:\s*"?(\w+)"?', f.read_text(), re.M)
    return m is not None and m.group(1) == val


def _has_userid(f: Path) -> bool:
    return re.search(r"^user_id:", f.read_text(), re.M) is not None


def verify(ws: Path, base: Path) -> int:
    fails: list[str] = []

    def check(ok: bool, msg: str) -> None:
        print(("PASS " if ok else "FAIL ") + msg)
        if not ok:
            fails.append(msg)

    sess = base / "sessions"
    check(sess.is_dir(), "base/sessions 存在")
    n_uuid = n_old = n_dele = 0
    if sess.is_dir():
        for d in sess.iterdir():
            if not d.is_dir():
                continue
            if UUID_RE.match(d.name):
                n_uuid += 1
                if (d / "delegation.json").exists():
                    n_dele += 1
            elif d.name.startswith(SESSION_PREFIX):
                n_old += 1
    check(n_old == 0, f"无 myclaw_s_ 前缀残留（{n_old}）")
    check(not (sess / "delegations").exists(), "delegations/ 已消除")
    print(f"INFO session 目录 {n_uuid}（含 delegation.json {n_dele}）")

    jobs = base / "jobs"
    if jobs.is_dir():
        dirs = [d for d in jobs.iterdir() if d.is_dir()]
        check(all((d / "meta.json").exists() for d in dirs), "每个 job 目录含 meta.json")
        n_hist = sum(1 for d in dirs if (d / "history.jsonl").exists())
        print(f"INFO jobs {len(dirs)}（含 history {n_hist}）")

    mem = base / "memory"
    mds = list(mem.glob("*.md")) if mem.is_dir() else []
    no_scope = [f.name for f in mds if not _has_scope(f)]
    check(not no_scope, f"memory 全部含 scope 键（缺失 {len(no_scope)}：{no_scope[:3]}）")
    n_user = sum(1 for f in mds if _scope_is(f, "user"))
    n_agent = sum(1 for f in mds if _scope_is(f, "agent"))
    print(f"INFO memory {len(mds)}（user {n_user} / agent {n_agent}）")
    check(all(_has_userid(f) for f in mds if _scope_is(f, "user")), "user scope 均含 user_id")

    # .legacy-rk-archive/.legacy-root-memory 是设计上就该保留的归档
    # （B11/B12 归档目标），底下带 memory/ 子树是预期状态，不算残留。
    users_dir = base / "users"
    LEGACY_ARCHIVE_DIRS = (".legacy-rk-archive", ".legacy-root-memory")
    stray_memory = [
        f for f in (users_dir.rglob("memory/*.md") if users_dir.is_dir() else [])
        if f.relative_to(users_dir).parts[0] not in LEGACY_ARCHIVE_DIRS
    ]
    check(not stray_memory, f"users/ 下无 memory 子树残留（{len(stray_memory)}）")
    # 上面那条按 "memory/*.md" 模式找残留，抓不到 .versions/{name}/v1.md 这种
    # （直接父目录不叫 memory）——B12 应该把整个 users/{ns} 子树清空，单独验证。
    check(not (base / "users" / NAMESPACE).exists(),
          f"users/{NAMESPACE} 已清壳（含 .versions/.audit/遗留 type 分区）")
    ws_mem = ws / "memory"
    check(not ws_mem.is_dir() or not any(ws_mem.iterdir()),
          "workspace/memory 内容已清空（若非空见上方 A4 归位动作）")
    for absent in ("sessions", "users", "memory", "agents", "skills", "backups"):
        check(not (ws / absent).exists(), f"workspace/{absent} 已不存在")
    check(not (ws / "cron" / "jobs.json").exists(), "workspace/cron/jobs.json 已不存在")
    check(not (ws / ".state" / "tasks.json").exists(), "workspace/.state/tasks.json 已不存在")

    print("\n" + ("对账通过 ✅" if not fails else f"对账失败 {len(fails)} 项 ❌"))
    return 0 if not fails else 1


def rollback(ws: Path, base: Path) -> None:
    check_daemon_stopped(base)
    mf = find_manifest(base)
    man = json.loads(mf.read_text())
    print(f"回滚依据：{mf}")
    bak = Path(man["backup"])
    if not bak.exists():
        sys.exit("错误：备份 bundle.tar.gz 不存在，无法回滚 modified 文件")
    # 1) 删除 created（拆分生成的文件）
    for c in reversed(man["created"]):
        p = Path(c)
        if p.exists():
            p.unlink()
            if p.parent.exists() and not any(p.parent.iterdir()):
                p.parent.rmdir()
            print(f"rm {c}")
    # 2) 先恢复 modified（tar 覆盖）——必须在逆 moves 之前：
    #    A1 的逆向 move 会把 base/backups（含 bundle 本身）搬回 ws，
    #    之后 bundle 路径即失效；且 modified 的 base 侧路径会随逆 move 消失
    with tarfile.open(bak) as tar:
        names = set(tar.getnames())
        for mpath in man["modified"]:
            p = Path(mpath)
            key = f"base/{rel(p, base)}" if p.is_relative_to(base) else f"ws/{rel(p, ws)}"
            if key in names and p.exists():
                member = tar.extractfile(key)
                if member:
                    p.write_text(member.read().decode())
                    print(f"restore {key}")
            elif key in names and not p.exists():
                print(f"跳过 restore（路径已随搬移变化）：{key}")
    # 3) 逆向 moves（后进先出）
    for mv in reversed(man["moves"]):
        src, dst = Path(mv["to"]), Path(mv["from"])
        if not src.exists():
            print(f"跳过（源不存在）：{src}")
            continue
        if dst.exists():
            print(f"警告：目标已存在，人工处理 {dst}")
            continue
        dst.parent.mkdir(parents=True, exist_ok=True)
        shutil.move(str(src), str(dst))
        print(f"move {src} → {dst}")
    print("\n回滚完成。请换回旧二进制后再启动 daemon。"
          "\n注意：base/backups 可能已随回滚整体迁回 workspace/backups（内含本次迁移备份）。")


def dry_run(ws: Path, base: Path) -> None:
    plan = build_plan(ws, base)
    if not plan.actions:
        print("无需迁移（数据已符合目标布局）。")
        return
    print(f"计划 {len(plan.actions)} 个动作（workspace={ws} base={base}）：\n")
    for i, a in enumerate(plan.actions, 1):
        if a.kind == "move":
            print(f"{i:4}. move   {a.src} → {a.dst}   [{a.note}]")
        elif a.kind == "created":
            print(f"{i:4}. create {a.dst}   [{a.note}]")
        elif a.kind == "modified":
            print(f"{i:4}. modify {a.src}（+scope: {a.meta.get('scope')}）   [{a.note}]")
        elif a.kind == "removed_dir":
            print(f"{i:4}. rmdir  {a.src}   [{a.note}]")
        elif a.kind == "mkdir":
            print(f"{i:4}. mkdir  {a.dst}   [{a.note}]")
        else:
            print(f"{i:4}. notify {a.note}")


def default_base_dir() -> Path:
    """必须与 Rust 侧 `default_base_dir()`（src/config/mod.rs，唯一权威来源，
    `src/migration.rs` 也委托给它）一致：`~/.myclaw`，不分平台。

    该函数一度实现成 XDG Base Directory 解析（`~/.local/share/myclaw`），
    对不上 daemon 实际使用的 `~/.myclaw`——用默认参数跑迁移会把数据搬到
    daemon 从不读取的目录，看起来像"数据全部消失/关联丢失"。Rust 侧后来
    把默认值统一改成了字面量 `~/.myclaw`（不再依赖平台相关的 XDG/Application
    Support 解析），这里跟着简化，两边不再可能因为平台判断细节分叉。
    """
    return Path.home() / ".myclaw"


def main() -> None:
    ap = argparse.ArgumentParser(description="MyClaw 存储布局迁移（P1）")
    ap.add_argument("--dry-run", action="store_true")
    ap.add_argument("--apply", action="store_true")
    ap.add_argument("--verify", action="store_true")
    ap.add_argument("--rollback", action="store_true")
    ap.add_argument("--workspace", type=Path, default=Path.home() / ".myclaw" / "workspace")
    ap.add_argument("--base", type=Path, default=default_base_dir())
    args = ap.parse_args()
    ws, base = args.workspace.resolve(), args.base.resolve()
    if sum([args.dry_run, args.apply, args.verify, args.rollback]) != 1:
        ap.error("四选一：--dry-run / --apply / --verify / --rollback")
    if args.dry_run:
        dry_run(ws, base)
    elif args.apply:
        apply(ws, base)
    elif args.verify:
        sys.exit(verify(ws, base))
    elif args.rollback:
        rollback(ws, base)


if __name__ == "__main__":
    main()
