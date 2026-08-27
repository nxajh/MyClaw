# RFC: webui/client.rs 拆分 — 2852 行 → client/ 目录 10 文件（P0 探路批）

> 状态：实施中（legacy-file-split-plan.md P0）
> 日期：2026-08-28
> 原则：纯移动重构，零行为变更；语句顺序逐句保持；CI-only 验证（禁本地 cargo）

## 1. 现状剖面（2026-08-28 实测，master 0c00096）

| 源行段 | 符号 | 去向 |
|--------|------|------|
| 43–174 | Subscriber / SessionOutputBus + impl | bus.rs |
| 175–1035 | ClientConnection / ClientChannel / impl ClientChannel | channel.rs |
| 1036–1074 | bus_key_candidates | bus.rs |
| 1075–1234 | impl Channel for ClientChannel | turn.rs |
| 1235–1283 | ClientTurnStream + impl TurnStream + impl Drop | turn.rs |
| 1284–1316 | is_safe_skill_name / resolve_skill_dir / reload_skills_from_workspace | api/skills.rs |
| 1317–1334 | ApiContext | api/mod.rs |
| 1335–1369 | memory_scope_dir / memory_file_in_scope / memory_user_id | api/memory.rs |
| 1370–2232 | handle_api_request（863 行，字符串路由 match） | api/mod.rs 骨架 + 域文件 |
| 2233–2390 | reconstruct_history | api/sessions.rs（sessions.history 的实现辅助） |
| 2391–2852 | mod tests（462 行，`use super::*`） | tests.rs |

**路由分域**（handle_api_request 内 match 臂，行号为 sed 偏移+1370）：
- sessions.list/create/switch/delete/delete_message/rename（1392–1595）+ sessions.history（~2160 段）→ api/sessions.rs
- tools.list（1596–1604，9 行）→ 留 api/mod.rs
- memory.list/write/delete/read（1605–1900）→ api/memory.rs
- file.read（1901）+ models.list/set + config.get/get_raw/save + commands.list + daemon.restart（~1930–2160）→ api/system.rs
- skills.list/read/write/delete（尾部）→ api/skills.rs

**外部消费者（唯一）**：`daemon/mod.rs:746/748` 经 `webui/mod.rs` 的 `pub use client::ClientChannel` —— client/mod.rs 转发 `pub use channel::ClientChannel` 后，两级 re-export 零改动。

## 2. 目标形态

```
src/webui/client/
├── mod.rs        声明 + re-export（ClientChannel/ClientTurnStream 等，~60）
├── bus.rs        Subscriber/SessionOutputBus/bus_key_candidates（~210）
├── channel.rs    ClientConnection/ClientChannel/impl ClientChannel（~870）
├── turn.rs       impl Channel/ClientTurnStream/impl TurnStream/impl Drop（~260）
├── api/
│   ├── mod.rs    ApiContext + handle_api_request 骨架 + tools.list + 兜底（~280）
│   ├── sessions.rs 路由臂 + reconstruct_history（~390）
│   ├── memory.rs   路由臂 + memory_* 辅助（~380）
│   ├── skills.rs   路由臂 + skills_* 辅助（~250）
│   └── system.rs   file.read/models/config/commands/daemon 路由臂（~280）
└── tests.rs      （~465，use super::* 经 mod.rs 转发面）
```

**阈值自检**：最大文件 channel.rs ~870（≤800 目标线略超、§5.3 硬线 1400 ✓；impl ClientChannel 860 行为连接+会话+订阅高耦合块，P0 不强二拆，留 RFC 记录）；其余全部 ≤465。总计 2852 行守恒（±导入行）。

## 3. 符号可见性

- `ClientChannel`：pub → mod.rs `pub use channel::ClientChannel`（webui/mod.rs 不动）
- `ClientTurnStream`：pub(crate) → mod.rs `pub(crate) use turn::ClientTurnStream`
- Subscriber/SessionOutputBus/ApiContext/各路由函数：文件私有或 pub(super)/pub(crate)，仅目录内可见
- bus_key_candidates/reconstruct_history/memory_* 群：pub(super)（被兄弟文件调用）

## 4. 批次（每批一 commit 一轮 CI）

1. **bus.rs**：43–174 + 1036–1074 → client/bus.rs；client.rs 顶部加 `mod bus;` + use；原引用改路径。零依赖探路。
2. **api/**：辅助群 + handle_api_request 骨架 + 四域文件；路由臂体提取为 `pub(super) fn route_<域>(...)`（参数显式传，语句逐句保持）。
3. **channel.rs + turn.rs + 壳**：175–1035 → channel.rs、1075–1283 → turn.rs；client.rs 剩余内容（use + 声明 + re-export）收为 client/mod.rs，删除原 client.rs。
4. **tests.rs**：2391–2852 → client/tests.rs；mod.rs 加 `#[cfg(test)] mod tests;`。

## 5. 验收

- [ ] `find src/webui -name "*.rs" \| xargs wc -l`：无 ≥1400，channel.rs ≤900
- [ ] `#[test]`+`#[tokio::test]` 计数守恒（拆分前后）
- [ ] `git diff master -- src/daemon/ src/webui/mod.rs` 为空（外部零改动）
- [ ] CI 三绿 × 每批；module_score 基线（2852/12pub/39privfn）vs 终态进 PR 描述

## 6. 风险

- 路由臂提取函数的参数捕获面大（ctx/params/共享局部）→ 批 2 若签名复杂，允许臂体整体保留在域文件中由 mod.rs 调用单函数（`route(ctx, action, params)` 按 action 前缀分发），仍是纯移动
- tests 的 `use super::*` 依赖 mod.rs 转发面完整 → 批 4 前逐符号 grep tests 体
