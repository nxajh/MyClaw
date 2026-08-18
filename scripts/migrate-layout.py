#!/usr/bin/env python3
"""MyClaw 存储布局迁移脚本（P1）。设计文档：docs/storage-layout-and-trigger-redesign.md §5。

旧布局（实体数据在 workspace 下）→ 新布局（sessions/users/jobs/memory 等归 data dir，
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


def inject_frontmatter(text: str, scope: str, user_id: str | None = None) -> str | None:
    """注入 scope（+user_id）到 frontmatter；已有 scope 键则返回 None（幂等）。"""
    lines = text.splitlines(keepends=True)
    has_fm = bool(lines) and lines[0].strip() == "---"
    keys: list[str] = [f'scope: "{scope}"']
    if user_id:
        keys.append(f'user_id: "{user_id}"')

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
        if find_key(fm, "scope"):
            return None
        inject = [k + "\n" for k in keys if not find_key(fm, k.split(":")[0])]
        return "".join(lines[:end]) + "".join(inject) + "".join(lines[end:])
    return "---\n" + "".join(k + "\n" for k in keys) + "---\n\n" + text


# ── 前置自检 ────────────────────────────────────────────────────────────────


def check_daemon_stopped(data: Path) -> None:
    pid_file = data / "myclaw.pid"
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


# ── 计划构建 ────────────────────────────────────────────────────────────────


def build_plan(ws: Path, data: Path) -> Plan:
    p = Plan()

    def W(*parts: str) -> Path:
        return ws.joinpath(*parts)

    def D(*parts: str) -> Path:
        return data.joinpath(*parts)

    # ── A 组：目录搬迁（workspace → data dir） ──
    # A1 backups 先搬（tar 备份要落在 data/backups/pre-layout/ 里）
    if W("backups").exists() and not D("backups").exists():
        p.add(kind="move", src=W("backups"), dst=D("backups"), note="A1 backups/ → data")
    if W("sessions").exists() and not D("sessions").exists():
        p.add(kind="move", src=W("sessions"), dst=D("sessions"), note="A2 sessions/ → data")
    if W("users").exists() and not D("users").exists():
        p.add(kind="move", src=W("users"), dst=D("users"), note="A3 users/ → data")
    # A4 memory：逐 md 合并进 data/memory（data 侧可能已有少量文件）
    if W("memory").is_dir():
        for f in sorted(W("memory").glob("*.md")):
            dst = D("memory", f.name)
            if dst.exists():
                sys.exit(f"错误：memory 名称冲突 {f.name}（data/memory 已存在同名文件）——人工决断后重跑")
            p.add(kind="move", src=f, dst=dst, note="A4 memory 平铺合并")
    if W("memory").is_dir() and not any(W("memory").glob("*.md")):
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
            p.add(kind="created", dst=D("jobs", m.group(0), "meta.json"),
                  note="A5 jobs.json 拆分", meta={"entry": e})
        rl = W("cron", "run_logs")
        if rl.is_dir():
            for f in sorted(rl.iterdir()):
                m = UUID_ANY_RE.search(f.name)
                if m:
                    p.add(kind="move", src=f, dst=D("jobs", m.group(0), "history.jsonl"),
                          note="A5 run_logs → history.jsonl")
                else:
                    print(f"警告：run_logs 无法解析 uuid，跳过 {f.name}")
        p.add(kind="move", src=jobs_json,
              dst=D("backups", "pre-layout", "jobs.json.bak"), note="A5 旧 jobs.json 归备份")
    # A6 agents / skills
    for name in ("agents", "skills"):
        if W(name).exists() and not D(name).exists():
            p.add(kind="move", src=W(name), dst=D(name), note=f"A6 {name}/ → data")
    # A7 旧 sessions 归档批次目录
    for d in sorted(ws.glob("sessions.*-archive*")):
        if d.is_dir():
            p.add(kind="move", src=d, dst=D("sessions", ".legacy", d.name),
                  note="A7 归档批次 → .legacy")
    # A8 .state：tasks.json → sessions/.legacy；其余 → data/state
    #
    # 目标已存在（常见场景：新代码已经在 data/state 直接跑起来一段时间，
    # workspace/.state 那份是重构前的遗留数据）不再整体报错退出——逐文件
    # 合并，已存在的目标条目原样跳过（execute_action 的 move 本来就是这个
    # 语义，这里只是让目录级冲突也享受同样的幂等 skip，而不是卡住整个迁移）。
    # 目录搬完后 removed_dir 会自底向上清理空壳；若真有文件级冲突，会保留
    # 该文件、打印警告，交给人工确认，而不是静默覆盖。
    if W(".state").is_dir():
        for f in sorted(W(".state").iterdir()):
            if f.name.startswith("tasks.json"):
                p.add(kind="move", src=f, dst=D("sessions", ".legacy", f.name),
                      note="A8 全局任务板归档（P1 起任务板 per-session）")
            elif f.is_dir():
                # 先保证目标目录本身存在——目录可能是空的（没有文件触发
                # mkdir(parents=True)），但仍需要在 data 侧占位存在。
                p.add(kind="mkdir", dst=D("state", f.name), note="A8 目录合并占位")
                for child in sorted(f.rglob("*")):
                    if child.is_dir():
                        continue
                    p.add(kind="move", src=child, dst=D("state", f.name, str(child.relative_to(f))),
                          note="A8 .state 运行时状态 → data/state（目录合并）")
                p.add(kind="removed_dir", src=f, note="A8 .state 子目录清壳")
            else:
                p.add(kind="move", src=f, dst=D("state", f.name),
                      note="A8 .state 运行时状态 → data/state")
        p.add(kind="removed_dir", src=W(".state"), note="A8 .state 清壳")

    # ── B 组：实体形态迁移（data dir 内） ──
    # 扫描位置带回退：A 组未执行时（dry-run / round1）实体仍在 ws 侧，扫等价位置
    def pick(name: str) -> Path:
        d, w = D(name), W(name)
        return d if d.is_dir() else (w if w.is_dir() else d)

    # B 组动作的 dst 一律指向 data 侧（src 用扫描侧：dry-run/round1 回退到 ws，
    # 执行时 round2 重建为 data 侧）
    d_sess = D("sessions")

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
                    for f in sorted(memdir.glob("*.md")):
                        dst = D("memory", f.name)
                        if dst.exists():
                            sys.exit(f"错误：memory 名称冲突 {f.name}（user 层迁移目标已存在）——人工决断")
                        p.add(kind="move", src=f, dst=dst, note="B12 user memory 平铺",
                              meta={"scope": "user", "user_id": f"{NAMESPACE}/u/{udir.name}"})
            dbl = main / NAMESPACE / "u"
            if dbl.is_dir():
                for udir in sorted(dbl.iterdir()):
                    if udir.is_dir() and UUID_RE.match(udir.name):
                        memdir = udir / "memory"
                        if not memdir.is_dir():
                            continue
                        for f in sorted(memdir.glob("*.md")):
                            dst = D("memory", f.name)
                            if dst.exists():
                                print(f"警告：双前缀 memory 同名跳过 {f.name}")
                                continue
                            p.add(kind="move", src=f, dst=dst, note="B12 双前缀并入主用户",
                                  meta={"scope": "user", "user_id": f"{NAMESPACE}/u/{udir.name}"})
            root_mem = main / "root" / "memory"
            if root_mem.is_dir():
                for f in sorted(root_mem.glob("*.md")):
                    p.add(kind="move", src=f, dst=D("users", ".legacy-root-memory", f.name),
                          note="B12 root 3 md 归档（不注入）")
        if (users / NAMESPACE).exists():
            p.add(kind="removed_dir", src=users / NAMESPACE,
                  note="B12 users/myclaw 清壳（.legacy-rk-archive 保留）")
    # B13 agent 层 memory 补 scope: agent（凡无 scope 键的；含 A4 未执行时的 ws 侧）
    mem_dir = pick("memory")
    if mem_dir.is_dir():
        for f in sorted(mem_dir.glob("*.md")):
            txt = f.read_text()
            if inject_frontmatter(txt, "agent") is not None:
                p.add(kind="modified", src=f, note="B13 agent memory 补 scope",
                      meta={"scope": "agent"})
    # B14 users.json（若存在）拆分为 users/{uuid}/meta.json
    uj = D("users.json")
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
            p.add(kind="created", dst=D("users", m.group(0), "meta.json"),
                  note="B14 users.json 拆分", meta={"entry": body})
        p.add(kind="move", src=uj, dst=D("backups", "pre-layout", "users.json.bak"),
              note="B14 旧 users.json 归备份")
    # B15 heartbeat 提示（不修改 TOML）
    toml = data / "myclaw.toml"
    if toml.exists() and "[scheduler.heartbeat]" in toml.read_text():
        p.add(kind="notify",
              note="B15 检测到 [scheduler.heartbeat] 配置：P3 将删除该机制，请手动移除该配置段")

    return p


# ── 执行 ────────────────────────────────────────────────────────────────────


def make_backup(ws: Path, data: Path, plan: Plan, bak_dir: Path) -> Path:
    """备份被 in-place 修改/拆分的小文件（rename 类不备份，靠 manifest 逆向）。"""
    bundle = bak_dir / "bundle.tar.gz"
    if bundle.exists():
        print(f"备份已存在，保留首轮：{bundle}")
        return bundle
    bak_dir.mkdir(parents=True, exist_ok=True)
    targets: list[tuple[Path, str]] = []
    for a in plan.actions:
        if a.kind == "modified" and a.src:
            if a.src.is_relative_to(data):
                targets.append((a.src, f"data/{rel(a.src, data)}"))
            else:
                targets.append((a.src, f"ws/{rel(a.src, ws)}"))
    # round2 时实体已在 data 侧原位
    extra = [data / "sessions" / "active.json", data / "sessions" / "delegations"]
    for e in extra:
        if e.is_file():
            targets.append((e, f"data/{rel(e, data)}"))
        elif e.is_dir():
            for f in e.rglob("*.json"):
                targets.append((f, f"data/{rel(f, data)}"))
    with tarfile.open(bundle, "x:gz") as tar:
        for p, arc in targets:
            if p.exists():
                tar.add(p, arcname=arc)
    return bundle


def apply(ws: Path, data: Path) -> None:
    check_daemon_stopped(data)
    bak_dir = data / "backups" / "pre-layout"
    manifest: dict[str, Any] = {
        "created_at": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
        "workspace": str(ws), "data": str(data),
        "moves": [], "created": [], "modified": [], "removed_dirs": [], "notifies": [],
        "backup": str(bak_dir / "bundle.tar.gz"),
    }
    total = 0
    try:
        # 两轮构建：round1 搬 A 组（workspace → data）；round2 时 B 组扫描才能看到
        # 已落位的实体（B9 扫 data/sessions、B12 扫 data/users、B13 扫 data/memory）。
        for rnd in (1, 2):
            plan = build_plan(ws, data)
            if not plan.actions:
                break
            if rnd == 2:
                # B 组执行前打 tar：modified 源文件此刻均在 data 侧原位
                bundle = make_backup(ws, data, plan, bak_dir)
                print(f"备份完成：{bundle}")
            for a in plan.actions:
                n = execute_action(ws, data, a, manifest)
                total += n
    except Exception as e:  # noqa: BLE001 —— fail-fast：报告后中止
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


def execute_action(ws: Path, data: Path, a: Action, manifest: dict[str, Any]) -> int:
    """执行单个动作；返回 1=执行 / 0=跳过。"""
    if a.kind == "move":
        dst = a.dst
        if dst.exists():
            print(f"跳过（目标已存在）：{a.note} {dst}")
            return 0
        if not a.src.exists():
            # round1 构建的 B 组动作：A 组搬移后 src 已变（data 侧），round2 重建执行
            print(f"跳过（源已移动，待下轮）：{a.note} {a.src}")
            return 0
        dst.parent.mkdir(parents=True, exist_ok=True)
        shutil.move(str(a.src), str(dst))
        manifest["moves"].append({"from": str(a.src), "to": str(dst)})
        print(f"move {a.src} → {dst}")
        # B12/B13 语义：move 携带 scope meta 时，落位后注入 frontmatter
        if "scope" in a.meta:
            txt = dst.read_text()
            new = inject_frontmatter(txt, a.meta["scope"], a.meta.get("user_id"))
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
        print(f"create {rel(a.dst, data)}")
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
        print(f"mkdir {rel(a.dst, data)}")
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


def find_manifest(data: Path) -> Path:
    base = data / "backups"
    cands = sorted(base.glob("pre-layout*/manifest.json")) if base.is_dir() else []
    if not cands:
        sys.exit(f"错误：未找到迁移 manifest（{base}/pre-layout*/manifest.json）")
    return cands[-1]


# ── 对账 / 回滚 ─────────────────────────────────────────────────────────────


def _has_scope(f: Path) -> bool:
    return re.search(r"^scope:", f.read_text(), re.M) is not None


def _scope_is(f: Path, val: str) -> bool:
    m = re.search(r'^scope:\s*"?(\w+)"?', f.read_text(), re.M)
    return m is not None and m.group(1) == val


def _has_userid(f: Path) -> bool:
    return re.search(r"^user_id:", f.read_text(), re.M) is not None


def verify(ws: Path, data: Path) -> int:
    fails: list[str] = []

    def check(ok: bool, msg: str) -> None:
        print(("PASS " if ok else "FAIL ") + msg)
        if not ok:
            fails.append(msg)

    sess = data / "sessions"
    check(sess.is_dir(), "data/sessions 存在")
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

    jobs = data / "jobs"
    if jobs.is_dir():
        dirs = [d for d in jobs.iterdir() if d.is_dir()]
        check(all((d / "meta.json").exists() for d in dirs), "每个 job 目录含 meta.json")
        n_hist = sum(1 for d in dirs if (d / "history.jsonl").exists())
        print(f"INFO jobs {len(dirs)}（含 history {n_hist}）")

    mem = data / "memory"
    mds = list(mem.glob("*.md")) if mem.is_dir() else []
    no_scope = [f.name for f in mds if not _has_scope(f)]
    check(not no_scope, f"memory 全部含 scope 键（缺失 {len(no_scope)}：{no_scope[:3]}）")
    n_user = sum(1 for f in mds if _scope_is(f, "user"))
    n_agent = sum(1 for f in mds if _scope_is(f, "agent"))
    print(f"INFO memory {len(mds)}（user {n_user} / agent {n_agent}）")
    check(all(_has_userid(f) for f in mds if _scope_is(f, "user")), "user scope 均含 user_id")

    check(not any((data / "users").rglob("memory/*.md")) if (data / "users").is_dir() else True,
          "users/ 下无 memory 子树残留")
    for absent in ("sessions", "users", "memory", "agents", "skills", "backups"):
        check(not (ws / absent).exists(), f"workspace/{absent} 已不存在")
    check(not (ws / "cron" / "jobs.json").exists(), "workspace/cron/jobs.json 已不存在")
    check(not (ws / ".state" / "tasks.json").exists(), "workspace/.state/tasks.json 已不存在")

    print("\n" + ("对账通过 ✅" if not fails else f"对账失败 {len(fails)} 项 ❌"))
    return 0 if not fails else 1


def rollback(ws: Path, data: Path) -> None:
    check_daemon_stopped(data)
    mf = find_manifest(data)
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
    #    A1 的逆向 move 会把 data/backups（含 bundle 本身）搬回 ws，
    #    之后 bundle 路径即失效；且 modified 的 data 侧路径会随逆 move 消失
    with tarfile.open(bak) as tar:
        names = set(tar.getnames())
        for mpath in man["modified"]:
            p = Path(mpath)
            key = f"data/{rel(p, data)}" if p.is_relative_to(data) else f"ws/{rel(p, ws)}"
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
          "\n注意：data/backups 可能已随回滚整体迁回 workspace/backups（内含本次迁移备份）。")


def dry_run(ws: Path, data: Path) -> None:
    plan = build_plan(ws, data)
    if not plan.actions:
        print("无需迁移（数据已符合目标布局）。")
        return
    print(f"计划 {len(plan.actions)} 个动作（workspace={ws} data={data}）：\n")
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


def default_data_dir() -> Path:
    """必须与 Rust 侧 `default_data_dir()`（src/config/mod.rs、src/migration.rs）
    一致：`directories::ProjectDirs::from("", "", "myclaw").data_dir()`。

    该函数曾硬编码为 `~/.myclaw`，与 daemon 实际解析出的路径（Linux 下遵循
    XDG Base Directory：`~/.local/share/myclaw`）不一致——用默认参数跑迁移会把
    数据搬到 daemon 从不读取的目录，看起来像"数据全部消失/关联丢失"。
    """
    if sys.platform == "darwin":
        return Path.home() / "Library" / "Application Support" / "myclaw"
    if sys.platform.startswith("win"):
        appdata = os.environ.get("APPDATA")
        base = Path(appdata) if appdata else Path.home() / "AppData" / "Roaming"
        return base / "myclaw" / "data"
    xdg_data_home = os.environ.get("XDG_DATA_HOME")
    base = Path(xdg_data_home) if xdg_data_home else Path.home() / ".local" / "share"
    return base / "myclaw"


def main() -> None:
    ap = argparse.ArgumentParser(description="MyClaw 存储布局迁移（P1）")
    ap.add_argument("--dry-run", action="store_true")
    ap.add_argument("--apply", action="store_true")
    ap.add_argument("--verify", action="store_true")
    ap.add_argument("--rollback", action="store_true")
    ap.add_argument("--workspace", type=Path, default=Path.home() / ".myclaw" / "workspace")
    ap.add_argument("--data", type=Path, default=default_data_dir())
    args = ap.parse_args()
    ws, data = args.workspace.resolve(), args.data.resolve()
    if sum([args.dry_run, args.apply, args.verify, args.rollback]) != 1:
        ap.error("四选一：--dry-run / --apply / --verify / --rollback")
    if args.dry_run:
        dry_run(ws, data)
    elif args.apply:
        apply(ws, data)
    elif args.verify:
        sys.exit(verify(ws, data))
    elif args.rollback:
        rollback(ws, data)


if __name__ == "__main__":
    main()
