# RFC: Skills 两层作用域 — 用户层 / agent 层

- **状态**: 已定稿待实施
- **日期**: 2026-08-21
- **关联**: issue #89（draft 积压可见性，PR #100）、#83/#85（共享库）、#93/#99（共享库写保护）、PR #100 评审备注（draft 名单跨用户暴露）、`docs/rfc-two-tier-memory.md`（记忆两层既有 frontmatter 语义，§6 将其存储同构化）
- **决策人**: 用户（nxajh），2026-08-21 会话定稿
- **修订**: 2026-08-29（决策人 nxajh）——「agent 层写权限」由"保持全体可写"改为 **fork 模型**：原始技能（agent 层/共享库）经 `skill_manage` 只读；修改经**惰性 fork** 落本人 user 层副本（详见 §2.6）。推翻原因：对话场景（用户要求 agent 改进共享 skill）下"全体可写"无法防御单用户改坏全员技能且无恢复手段；fork 保留零摩擦改进循环，破坏半径收敛到副本，原始版即恢复基线。§2.4/§2.5/§5 同步更新。

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
| agent 层写权限 | **fork 模型**（2026-08-29 修订）：原始技能经 `skill_manage` 只读；修改自动 fork 到本人 user 层副本（遮蔽生效）。更新原始版走 operator 的文件系统/git 或 P3 `promote`（`~/.agents/skills` 共享库 #99 保护不变，同为 fork 源） |
| extract 默认落点 | user 层（从谁的会话提取落谁的层）。无主会话**不提取**（owner 缺失视为 bug，warn + skip，不写任何层，2026-08-29 P2 修订）；headless 身份经 CLI `--user` / `[system] operator`、cron 经 `JobEntry.creator` 补齐 |

## 2. 设计

### 2.1 存储布局

```
{base_dir}/skills/                          # agent 层（语义收窄为显式 agent 层，结构不变）
{base_dir}/users/{uuid}/skills/             # user 层（P1 同构三实体的自然延伸）
~/.agents/skills/                           # 跨 agent 共享库（不变，#83/#99）
```

- **目录权威，不增加顶层 Frontmatter**：遵循 Agent Skills 规范（#123，不自造顶层字段），技能层归属**完全由物理目录位置决定**，不需要在 frontmatter 中冗余 `scope` 和 `user_id`（有别于记忆系统的处理方式）。
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

### 2.3 skill_extract 落点（源头关闭泄露；2026-08-29 P2 修订）

- `SkillExtractInput` 携带 session owner（#100 加了 channel/reply_target，P2 同处加
  `owner_fqid`）→ `ToolContext.owner` 一律落 owner 的 user 层
  （`users/{uuid}/skills/`）；extract 的去重索引同时覆盖 owner user 层 + agent 层
  （同名 user 层优先），与写入视角一致。
- headless / cron / 无 owner 会话：**不再落 agent 层**（P2 修订）。owner 缺失 =
  调用方 bug，fork 直接 warn + 返回（不写任何层）。身份补齐路径：
  - CLI：`myclaw exec --user <username|uuid>`，未给时取 `[system] operator`
    （`[system]` 新增 `operator` 配置，支持 FQID/裸 uuid/username，username 经
    `users/*/meta.json` 静态反查归一化为 uuid）；
  - cron：`JobEntry` 新增 `creator` 字段（创建时经 UserResolver 归一化记录），
    scheduler 触发时经 `CronTrigger.creator` 带出，Isolated `_job_*` 会话（owner
    未归属）据此设置 `session.owner_fqid`；Inject 模式注入用户会话，owner 已由
    load_session 正确解析，不覆盖。
  - draft 状态不变。
- #100 层② backlog 提醒按层分账（P2 实施）：user 层积压只注入该 owner 会话
  （`users/{uuid}/skill_draft_reminder_state.json` 独立按日节流，满 5 触发）；
  agent 层积压（`{base_dir}/skill_draft_reminder_state.json`，兼容原位置）仅注入
  operator 会话（`[system] operator` 归一化比对 `session.owner_fqid`），文案注明
  "agent layer drafts"（收编 PR #100 评审备注 issue）。

### 2.4 审核动词与晋升

triage 词表（保留/合并/删除）增加「**提升**」：

- user→agent 晋升即审核动作本身：`skill_manage` 新 action `promote`（operator 或
  技能 owner 可发起；晋升后原始版仍只读，其他用户的改进同样经 fork→再 promote 循环，信任模型一致）。
- 晋升时去标识化检查（同 memory agent-scope 规则）：提示词正文不得含 user_id、
  个人路径、会话专属引用；由执行者（agent）自查 + 提示确认。

### 2.5 写权限矩阵

| 层 | owner | operator | 其他用户 |
|---|---|---|---|
| user 层（本人） | 读/写 | 读 | 不可见 |
| agent 层 | — | 只读*（fork 源） | 只读（fork 源） |
| 共享库 `~/.agents/skills` | — | 只读（#99，fork 源） | 只读（#99，fork 源） |

*operator 更新原始版走 workspace 文件系统/git（或 P3 `promote`）；`skill_manage` 通道一律 fork（2026-08-29 修订）。

### 2.6 fork 模型（2026-08-29 增补，P1.1 实施）

`skill_manage` 写操作（edit/patch/delete/write_file/remove_file）目标解析规则：

- 目标在本人 user 层 → 直接写（现状不变）。
- 目标在 agent 层 / 共享库（合成读命中非 user 层）→ **惰性 fork**：完整复制技能目录
  （SKILL.md + 全部子目录）到本人 user 层，随后在副本上执行原操作；返回信息注明
  「已创建你的副本，原始版未动」。
- `delete` 对非 user 层目标直接拒绝（fork 后删副本无意义），提示该技能为共享原始技能。
- 副本来源记录：sidecar 文件 `.fork-origin`（JSON：源层、源路径、fork 时间戳），不写
  SKILL.md 正文（目录权威原则）。
- 动机：零摩擦改进循环 + 破坏半径收敛到副本 + 原始版即恢复基线（副本改坏 → 删副本
  重新 fork）。与 §6.2 记忆写语义（默认落 owner 层）对齐。

### 2.7 修订记录

- 2026-08-29 P2（本 PR）：§1 决策表 + §2.3 修订——extract 一律落 owner user 层，
  无主会话不提取；headless 身份补齐（CLI `--user` / `[system] operator` /
  `JobEntry.creator`）；backlog 提醒按层分账（scope 独立节流，agent 层仅 operator
  可见）。§5 对应项勾选。
- 2026-08-29 P1.1：§2.6 fork 模型增补（skill_manage 写路径惰性 fork）。
- 2026-08-21 初稿；2026-08-28 §6 记忆分拆定稿。

## 3. 存量迁移

- 范围：实测 21 个（4 draft + 17 active；PR #100 声称 36/31 与实测不符，按实测计，
  差异疑为作者计数口径或其间已部分 triage，不影响迁移方案）。
- 目标：全部 → **operator 的 user 层**（`users/{operator_uuid}/skills/`）。
  注：其他用户在提升发生前将看不到这些技能（SkillListing 变化），这是决策的
  已知代价；如需保留个别技能全员可见，迁移后逐个 `promote` 即可。
- 方式：停机迁移脚本（镜像 `migrate-layout.py` 模式）：
  1. 建 `users/{operator_uuid}/skills/`；
  2. `git mv` 语义搬移 21 个目录至新层，不改动 frontmatter（由目录定层）；
  3. 校验：`list_draft_skill_names` / `load_skills_from_dir` 对新位置计数一致；
  4. 回滚：脚本幂等，支持 `--rollback`。

## 4. 实施切分

| 阶段 | 内容 | 依赖 |
|---|---|---|
| P1 | 存储布局 + loader 三层合成 + 迁移脚本 | #100 合并（backlog 分账基于其提醒机制） |
| P2 | extract 落 user 层 + backlog 按层分账 | P1 |
| P3 | `promote` action + 去标识化检查 + watcher user 层监听 | P1 |
| P4 | 记忆存储分拆（§6）+ 迁移脚本 | P1（复用 `users/{uuid}/` 布局与迁移脚本模式，可与 P2/P3 并行） |

## 5. 测试清单

- [ ] loader：三层合成、同名优先级（user>agent>共享库）、无 user 上下文（子代理）视角
- [x] extract：user 会话落 user 层；headless 无主不提取（warn+skip，P2 修订）；draft 状态保持
- [x] backlog 提醒：user 层积压注入本人；agent 层积压仅 operator 可见；互不越层；scope 独立节流（P2）
- [ ] promote：owner/operator 可发起；去标识化检查提示；晋升后 agent 层可写语义
- [ ] skill_manage：user 层本人五操作可写；其他用户对 user 层得到 not-found（不可见性）
- [ ] fork：edit/patch/write_file 对 agent 层目标自动建副本后写入、原始版逐字节不变；delete 对非 user 层拒绝；`.fork-origin` 生成；副本遮蔽生效
- [ ] 迁移脚本：21 个计数守恒、draft/active 状态守恒、幂等、回滚
- [ ] watcher：user 层技能增删热载

## 6. 记忆存储分拆（同构延伸，已定稿）

记忆两层目前仅靠 frontmatter 判定（`scope`+`user_id`），物理仍是单池
`{base_dir}/memory/`。技能层定型后，记忆存储同构分拆为目录分层。本节规则与
§2 惯例对齐；uuid 与条目身份问题经三轮讨论定稿（2026-08-21，决策人 nxajh）。

### 6.0 设计哲学：多段人际关系模型（2026-08-28 确立）

分层的根本依据是 MyClaw 多租户底层哲学的确立：**Agent 不是 SaaS 隔离沙箱，而是有统一心智、处理多段人际关系的“真实个人”**。
- **Agent 层（`memory/`、`skills/`）** = Agent 的本体心智、世界观与全局经验池（不含任何用户隐私）。
- **User 层（`users/{uuid}/memory/`）** = Agent 脑中关于特定用户的专属关系档案。
- **经验内化（Promote / Distill）** = Agent 将服务单个用户的特殊经验，去标识化后内化为自我成长，用于服务所有人。
物理目录隔离完美契合此哲学：运行时拼装 `Agent本体 + User档案`，物理层隔离杜绝了 LLM 跨档案串联引发的隐私幻觉。

### 6.1 存储布局与身份

```
{base_dir}/memory/                          # agent 层
{base_dir}/users/{uuid}/memory/             # user 层
```

- **目录权威，frontmatter 冗余校验**：迁移后 `scan_merged` 按目录定层，不再靠
  解析 frontmatter 过滤；文件内 `user_id` 与目录归属不一致 → lint 告警。
- **user 层写入门槛**：`scope=user` 要求 user_id 为注册实体 FQID
  （`UserRegistry::find_by_uid` 命中）。`UserResolver` 无 override 时的
  routing_key fallback 身份（未 /link 的渠道来客）**拒写并引导 /register**——
  user 层数据必挂注册实体，不留半身份目录。fork（user 层主写入路径）对未注册
  owner 同样跳过 user 层写入，不阻断主流程。
- **条目身份 = `name`，不引入条目级 uuid**：name 承载语义与检索（前缀已是
  事实命名空间）；uuid 对 LLM 是不透明噪音，且 name+uuid 双身份体系是永久维护
  成本。uuid 的独有价值（rename/移动不断链）当前无触发场景——系统没有 rename
  操作，改名走 add-new + remove-old。将来 rename/promote 高频时再加 `id:` 字段
  增量迁移，工具接口不动（链接语法预留扩展位，见 6.3）。

### 6.2 同名与读写语义（name 按层分 namespace）

分拆后同名条目跨层合法共存（如 root 与 nxajh 各有 `travel_preferences`，
两层各有 `agent_github_identity`），工具面（memory_view / memory_manage /
memory_search）仍按 name 寻址，规则如下：

- **读**（list/view/search）：合并视图，同名 **user 遮蔽 agent**（同 §2.2
  "local overrides shared" 惯例）。
- **写**（replace/remove）：默认目标 = 会话 owner 的 user 层；name 仅存在于
  agent 层时须**显式 scope** 才可改，防误改共享层。
- **add**：目标层已有同名报错；user 层新增条目与 agent 层同名 = 合法遮蔽，
  但须提示、不得静默。
- **promote**（user→agent 去标识化提升）：目标层已有同名须先改名或合并。

### 6.3 链接（See Also）

实测 443 条目、1097 条有效链接，其中 **226 条（21%）跨未来层边界**（如
`agent:myclaw_three_track_scheduler → user:myclaw_overview`）——跨层链接是
主流现象，一等支持（"先禁止跨层引用"已否决）：

- 同层保持裸名：`[Related: x](x.md)`（现有语法不变）。
- 跨层用层限定语法：`[Related: x](agent:x.md)` / `[Related: x](user:x.md)`；
  解析器同时接受 uuid 形态 `@<uuid>` 作为预留扩展位（为将来 `id:` 字段铺路，
  现不启用）。
- 链接只在层内解析 + 层限定跨层解析；`memory_manage` 的裸目标 lint 扩展到
  层限定形态。

### 6.4 迁移与结构不变式强校验

- 范围（2026-08-21 实测）：443 个文件（user 层 195 个，全部属 operator
  01a0151d；nxajh 0 个；agent 层 248 个）。
- 方式：停机脚本（镜像 §3 模式）：
  1. frontmatter `user_id` → `UserRegistry` 查表 → `git mv` 语义搬移到
     `users/{uuid}/memory/`；
  2. 226 条跨层链接改写为层限定语法；
  3. 查不到注册实体的（防未来出现）进 `users/.pending/` 隔离人工审，不猜；
  4. **结构不变式强校验（Absolute Invariants）**：
     - 计数/分层守恒：443 计数、195/248 分层分布绝对一致。
     - **零死链闭合**：1083 条活链改写后，必须强制过解析器，悬空链接数必须为 0（不允许产生幽灵链接）。
     - **归属绝对一致**：文件的 `user_id` Frontmatter 必须与其所在的物理目录归属完全匹配，异常直接抛错拦截，不静默跳过。
     - 幂等 + `--rollback`，验证无误后方可执行。
- 现存 14 条死链（add+remove 式改名残留）迁移时一并清理或修复。

### 6.5 测试清单增补

- [ ] scope=user 写入：注册 FQID 通过；routing_key fallback 身份拒写并提示
      /register；fork 未注册 owner 跳过 user 层不阻断
- [ ] 同名遮蔽：读合并视图 user>agent；写默认 owner 层；agent 层同名须显式
      scope；user 层新增遮蔽 agent 层同名有提示
- [ ] 跨层链接：层限定语法解析；同层裸名不受影响；`@<uuid>` 预留位可解析
- [ ] 迁移结构不变式：计数/分层守恒；**全解析器零死链校验通过**；**Frontmatter-物理目录绝对一致性强制拦截生效**；隔离目录不静默丢弃

## 7. 未来展望（P5）：全权限自主反思的基座

当前 MyClaw 在 memory 层面已有基于 `.versions` 目录的覆盖快照保护，但在多文件协同修改（如反思代理同时更新 3 个 memory、生成 1 个 skill）时，自建轮子的版本控制在找错和回滚上成本过高。
在 #101 目录分拆完毕、哲学对齐“统一心智+关系档案”后，下一步（P5）应将底层存储切换为 Git-backed（如 Letta MemFS 的实践）。这将使 Agent 的后台 Reflection 从“受控提炼”迈向“免审核自主演进”（跑错只需一条 `git reset` 即可安全回滚，天然支持原子化批处理和 Diff 报告审计）。
