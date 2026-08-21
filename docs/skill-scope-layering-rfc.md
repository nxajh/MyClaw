# RFC: Skills 两层作用域 — 用户层 / agent 层

- **状态**: 已定稿待实施
- **日期**: 2026-08-21
- **关联**: issue #89（draft 积压可见性，PR #100）、#83/#85（共享库）、#93/#99（共享库写保护）、PR #100 评审备注（draft 名单跨用户暴露）
- **决策人**: 用户（nxajh），2026-08-21 会话定稿

## 0. 问题陈述

记忆系统已两层（`scope: user` 带 user_id / agent 层默认），技能系统仍单池：

1. **跨用户知识泄露**：`skill_extract` 从任何用户的会话提取 → 全体用户共享的
   `{base_dir}/skills`。用户 A 会话沉淀的流程自动对用户 B 生效（含话题线索）。
   PR #100 评审已确认同根问题存在于 draft backlog 提醒。
2. **无个性化空间**：同名需求（"查机票"）不同用户偏好流程应可不同，现无法分叉。
3. **晋升通道缺失**：记忆系统的 user→agent 晋升（去标识化后共享）在技能系统无对应。

## 1. 已定决策（用户拍板）

| 决策点 | 结论 |
|---|---|
| 分层模型 | 两层：user 层 + agent 层（对齐 memory 两层与 `load_skills_layered` 惯例） |
| 存量迁移 | **全部 21 个（4 draft + 17 active）归 user 层**，agent 层从空开始，此后仅经「提升」进入 |
| agent 层写权限 | **保持全体可写**（信任模型现状不变；#99 写保护仅针对 `~/.agents/skills` 共享库） |
| extract 默认落点 | user 层（从谁的会话提取落谁的层），不允许直接产 agent 层 draft |

## 2. 设计

### 2.1 存储布局

```
{base_dir}/skills/                          # agent 层（语义收窄为显式 agent 层，结构不变）
{base_dir}/users/{uuid}/skills/             # user 层（P1 同构三实体的自然延伸）
~/.agents/skills/                           # 跨 agent 共享库（不变，#83/#99）
```

- frontmatter 增加 `scope: user` + `user_id`（冗余于目录位置，保持跨层扫描一致性；
  缺省无 scope 字段时按目录位置判定——迁移后 agent 层不再需要 scope 字段）。
- user 层目录纳入 `users/{uuid}/` 实体布局（现仅 meta.json），与 sessions/jobs 同构。

### 2.2 加载与冲突

`load_skills_layered` 扩展为三层合成（user 视角）：

```
user 层 ∪ agent 层 ∪ 共享库
同名冲突优先级：user > agent > 共享库（沿用 "local overrides shared" 语义 + warn-once）
```

- `SkillListing` 组装按 session owner 视图合成；agent / 子代理（无 owner 用户上下文）
  视角 = agent 层 ∪ 共享库。
- `WorkspaceWatcher` 增加 user 层目录监听（热载同现有机制）。

### 2.3 skill_extract 落点（源头关闭泄露）

- `SkillExtractInput` 已有 session 归属（#100 加了 channel/reply_target，同处取
  owner user id）→ 写入 `users/{uuid}/skills/`。
- headless / cron / 无 owner 会话：落 agent 层（operator 上下文），draft 状态不变。
- #100 层② backlog 提醒按层分账：user 层积压只注入该用户会话；agent 层积压注入
  operator（收编 PR #100 评审备注 issue）。

### 2.4 审核动词与晋升

triage 词表（保留/合并/删除）增加「**提升**」：

- user→agent 晋升即审核动作本身：`skill_manage` 新 action `promote`（operator 或
  技能 owner 可发起；agent 层全体可写 ⇒ 晋升后其他用户可再修改，信任模型一致）。
- 晋升时去标识化检查（同 memory agent-scope 规则）：提示词正文不得含 user_id、
  个人路径、会话专属引用；由执行者（agent）自查 + 提示确认。

### 2.5 写权限矩阵

| 层 | owner | operator | 其他用户 |
|---|---|---|---|
| user 层（本人） | 读/写 | 读 | 不可见 |
| agent 层 | — | 读/写 | 读/写（决策：保持全体可写） |
| 共享库 `~/.agents/skills` | — | 只读 | 只读（#99） |

## 3. 存量迁移

- 范围：实测 21 个（4 draft + 17 active；PR #100 声称 36/31 与实测不符，按实测计，
  差异疑为作者计数口径或其间已部分 triage，不影响迁移方案）。
- 目标：全部 → **operator 的 user 层**（`users/{operator_uuid}/skills/`）。
  注：其他用户在提升发生前将看不到这些技能（SkillListing 变化），这是决策的
  已知代价；如需保留个别技能全员可见，迁移后逐个 `promote` 即可。
- 方式：停机迁移脚本（镜像 `migrate-layout.py` 模式）：
  1. 建 `users/{operator_uuid}/skills/`；
  2. `git mv` 语义搬移 21 个目录 + frontmatter 补 `scope: user` / `user_id`；
  3. 校验：`list_draft_skill_names` / `load_skills_from_dir` 对新位置计数一致；
  4. 回滚：脚本幂等，支持 `--rollback`。

## 4. 实施切分

| 阶段 | 内容 | 依赖 |
|---|---|---|
| P1 | 存储布局 + loader 三层合成 + 迁移脚本 | #100 合并（backlog 分账基于其提醒机制） |
| P2 | extract 落 user 层 + backlog 按层分账 | P1 |
| P3 | `promote` action + 去标识化检查 + watcher user 层监听 | P1 |

## 5. 测试清单

- [ ] loader：三层合成、同名优先级（user>agent>共享库）、无 user 上下文（子代理）视角
- [ ] extract：user 会话落 user 层；headless 落 agent 层；draft 状态保持
- [ ] backlog 提醒：user 层积压注入本人；agent 层积压注入 operator；互不越层
- [ ] promote：owner/operator 可发起；去标识化检查提示；晋升后 agent 层可写语义
- [ ] skill_manage：user 层本人五操作可写；其他用户对 user 层得到 not-found（不可见性）
- [ ] 迁移脚本：21 个计数守恒、draft/active 状态守恒、幂等、回滚
- [ ] watcher：user 层技能增删热载
