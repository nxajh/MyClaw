# RFC: 工位分离与多实例架构 — 身体/工位两域

- **状态**: Draft（会话收敛稿）——含未决分歧点，见 §9，待决策人批注后定稿
- **日期**: 2026-08-31
- **决策人**: 用户（nxajh）
- **来源**: 2026-08-31 架构讨论（agent 工具面 → 工位分离 → 多实例 → 委托模型退役）
- **修订**: 2026-09-01——Q6 解决：人格文件查证为废弃遗留（运行时零引用，语义已迁 `agents/main/AGENT.md` 与 `user_profile` 类型化字段），归属裁决问题撤销，改列清理项（§4.4/§8-P0）；2026-09-05——D13 已决：契约 W 记为原则区 endgame（§1.7/§6.4，issue #256）
- **关联**: `docs/skill-scope-layering-rfc.md`（RFC #101，分层先例）、`docs/worktree-delegation.md`（delegate 现状）、`src/cli/cmd_update.rs`（update 事务现状）、`src/cli/cmd_exec.rs`（exec 入口，§6 前置件已存在）

## 0. 问题陈述

现状中 agent 的**身体**（daemon 进程、状态目录、感官层）与**工位**（干活的执行环境）同体于 x86 宿主机，双向耦合，两个方向都有实际伤害记录：

1. **工位伤害身体**：工位内重活（如本地编译）OOM 可直接 kill daemon（`no-local-build-on-micro` 规则即此症状的行为层补丁——用规则防事故，说明机制层无防御）。
2. **身体伤害工位**：update 热切换 SIGUSR1 后 drain 活跃 turn，进行中 shell 进程表丢失，结果只能从 `.out` 文件考古（P4 迁移实测）——手术不打麻药，进行中的工作陪葬。

同时 MyClaw 的差异化押注是 **agent 自演进**（L4：agent 参与自己 harness 的修改与部署），而竞品实证（OpenClaw/Claude Code/Codex/grok-bot，2026-08-31 源码勘察）无一将 update 暴露给 agent——operator 域共识。自演进要安全闭环，前提是手术与干活隔离。

本 RFC 定义身体/工位两域分离、多实例架构、委托模型退役及自演进治理配速。

## 1. 核心原则

1. **身体无字节通道**：agent 对身体的每个操作都是签名工具；动作按对象授权、可审计、可枚举。
2. **工位可丢弃**：shell/文件/构建宿主是可即焚的容器工作负载；成果经 git/artifact 回流，不双向同步。
3. **契约保形**：任何经 backend 路由的能力，语义在通道两端等价（截断/超时/错误结构），否则路由层不许上线。
4. **验证回路外置**：CI 与 operator 审批在 agent 可自改范围之外（强度待批注，见 §7.2/§9-Q1）。
5. **bootstrap 归身体**：工位宿主与实例的起停由 daemon/systemd/宿主 launcher 持有，agent 不承担自救循环。
6. **自主是让渡不是获取**：各层自主档位由 operator 配置与调整（sudoers 类比：谁能 sudo 是配置）。
7. **悬停不伪造完成（契约 W，endgame）**：工具未完成，execute() 不得以非结果返回——阻塞至事件到达并返回事件本身，或跨重启时经重放取得真事件后返回；任何 stub/占位完成态均属违例，UI 须呈现"进行中"。结果由单一写入者落 history（endgame：工具自身；2b 落地中由会话层 park_for_yield 代行，见 §6.4）。挂起态的唯一权威源是持久化 history；任何执行可能跨越重启的工具自带重放幂等义务。现状为同任务 park 变体（过渡，§6.4，issue #256）。

## 2. 决策点总表

| # | 决策点 | 结论 | 状态 |
|---|---|---|---|
| D1 | 隔离技术选型 | Docker/远端，不做 userns（§10.1） | 会话收敛 |
| D2 | 工位宿主 | ARM oci-arm-1（12GB，Docker 已运行）；backend 抽象保证可迁云 | 会话收敛 |
| D3 | 容器形态 | 一种标准镜像 + ttl 参数化（即焚/常驻），不按角色预设容器分类 | 会话收敛 |
| D4 | shell 归属 | 工位域。身体终局无字节通道（过渡与收尾见 §8） | 会话收敛 |
| D5 | 身体域文件通道 | file_read/write/edit 对身体域状态路径结构化拒绝（§4.1） | 会话收敛 |
| D6 | task 统一体 | cronjob 并入 task：备忘录（pull）/提醒（push），trigger 为可选属性（§4.3） | 会话收敛 |
| D7 | 凭证模式 | 三模式按实例档位分配：代理（即焚）/独立 key（常驻）/共享主凭证（排除）（§5.4） | 会话收敛（默认档待批，§9-Q4） |
| D8 | delegate 前途 | 多实例架构下退役：被 exec + send_message + 宿主 launcher 吸收（§6） | 提案待批（§9-Q2/Q3） |
| D9 | inline 档 | 保留，域选择器定义（身体域子任务 inline、世界域子任务工位实例）（§6.2） | 提案待批（§9-Q5） |
| D10 | L4 部署闭环 | CI smoke canary（排练位进 CI）+ rehearsed 态 + 失败自动回退（§7.3） | 会话收敛 |
| D11 | 演进分层 | L1 记忆 / L2 技能 / L3 行为面 / L4 代码；每层提供自主档位（§7.1） | 会话收敛（档位上限待批，§9-Q8） |
| D12 | 消息路由 | 实例间默认经主 gateway 中转（复用 FQID 寻址/唤醒/审计）；P2P 直连为显式让渡档位 | 提案待批（§9-Q7） |
| D13 | sessions_yield 悬停契约 | 契约 W 记为原则区 endgame（§1.7/§6.4，配套：history 唯一权威源 + 重放幂等义务）；现状契约 Y 为过渡实现 | 已决（2026-09-05，issue #256） |

## 3. 架构总览

```
x86 本机 (954MB)              ARM oci-arm-1 (12GB)              GitHub CI
┌────────────────────┐        ┌─────────────────────────┐      ┌──────────┐
│ 身体：myclaw daemon │ ═SSH═> │ 工位宿主                 │      │ build +  │
│  感官：channels/    │ docker │  ├ launcher 薄服务       │      │ smoke    │
│   friends/jobs      │ API    │  │  (镜像白名单/配额/     │      │ canary   │
│  治理：body 守卫/    │        │  │   key 发放记录)        │      │ (手术    │
│   审计/自主档位      │        │  └ Docker(既有)          │      │  排练位) │
│  元工具：update/     │        │     └ 工位实例 ×N        │      └──────────┘
│   restart/status    │        │        (标准镜像, ttl)   │
│  实例生命周期管理     │        │                          │
└────────────────────┘        └─────────────────────────┘
   agent 工具面三域：身体工具(daemon 内) / 工位 shell+file(容器内) / 元工具
```

- 感官层不动（渠道/调度/记忆/身份/消息留在 operator 身边）。
- 工位实例是**完整 myclaw 实例**（自有 daemon、自有工具面），不是裸 shell 容器。
- zt-x86-1 为二期备用工位/外部观察哨（可选，不阻塞本 RFC）。

## 4. 身体域设计

### 4.1 body-domain 路径守卫

`file_ops`（file_read/file_edit/file_write/list_dir）入口统一 `resolve_and_guard(path)`：

1. **canonicalize 前置**（堵 symlink 与 `..` 穿越这两个标准绕法），再匹配身体域根：memory、`users/{uuid}`、skills、sessions、update-state、config。
2. 命中即结构化拒绝，错误信息带路由提示：`body-domain path; use memory_search / skill_view / session_query / self`。
3. 守卫防误用（fail-safe），不防越权——硬墙是 OS/容器边界（工位容器内无 `~/.myclaw`，物理不可见）。两层各司其职，工位分离完成时闭合。

**理由（写分离与读分离）**：写通道旁路 = 治理单点失效（audit/inject 策略/frontmatter 校验/scope 全部绕过）；读通道旁路 = 注入裁决权旁路 + 多租户下跨租户读的语义可能。专用工具的价值在通道独占，双通道存在则治理失效。

### 4.2 元工具族（新增）

| 工具 | 语义 |
|---|---|
| `daemon_update` | detached handoff：CI 已排练（rehearsed）artifact → rename swap（`.old` 留存）→ SIGUSR1 → exit。内置 `update_state` 状态机（加 `rehearsed` 态：仅 CI 排练过的 run_id 允许进 staged） |
| `daemon_restart` / `daemon_status` / `daemon_logs` | 结构化自省，替代 journalctl/进程表探活类 shell 诊断 |

`cmd_update.rs` 现有事务语义（幂等短路/防重启风暴/400ms stdout 冲刷/发信号即退出）整体保留，工具化只加签名层。

### 4.3 task 统一体（cronjob 并入）

```
Task { subject, details, parent?, trigger?, consumption, lifecycle }
  trigger:  None            → 备忘录（pull：agent 主动查/列表注入，不唤醒）
            At(t) | Cron    → 提醒（push：到点注入会话，唤醒执行）
            Webhook(filters)→ 事件提醒（push：外部事件触发）
  lifecycle: once | recurring(max_runs, delete_after_run)
```

- 现有 cronjob 的 schedule/webhook/delivery/max_runs 收编为 trigger/lifecycle 字段；cronjob 工具退役为 task 的触发器管理面。
- 分界本质是**消费模型**（pull vs push），非触发器有无。
- details 统一字段、两种渲染：提醒语义（触发时渲染为可执行 prompt）/备忘录语义（记录）。不拆两字段。
- 诊断先例：dsh U3a 正交模型（schedule 可选、去 kind）同方向验证。

### 4.4 身体域状态通道独占清单

| 状态 | 唯一通道 |
|---|---|
| memory | memory_* 工具 |
| skills | skill_manage / skill_view |
| sessions | session_query |
| daemon 生命周期 | 元工具族 |
| 迁移类操作（P4-memory 那类碰身体状态） | 受控 migration 元命令（复用 `--operator-fqid`/O_EXCL 锁语义），永不进工位 |

**已解决（2026-09-01）**：原拟裁决 workspace 人格文件（SOUL/IDENTITY/USER/AGENTS/BOOTSTRAP/HEARTBEAT.md）归属。查证结论：**废弃遗留，非活跃人格文件**——运行时零引用（`prompt.rs:94` 注释即裁决："AGENT.md body is the single source，builder 不再从磁盘读 USER.md"；`user_profile.rs:163` 记录 USER.md 内容已入类型化字段；`daemon/mod.rs:239` 仅存给旧用户的迁移提示），mtime 全部停在 2026-05 中旬。语义迁移路径完整：USER.md → `user_profile`；IDENTITY/SOUL → `agents/main/AGENT.md`；HEARTBEAT 体系已删（U1）。无归属问题，仅待清理：git 收档后删除（workspace 根为 git 仓库且该批文件未跟踪，先 add 入历史再删，保证可恢复），列 P0。

## 5. 工位域设计

### 5.1 SandboxBackend trait

```rust
trait SandboxBackend: Send + Sync {
    async fn exec(&self, ws: &WsId, spec: ExecSpec) -> Result<ExecResult>;      // 超时/30K 截断/.out 语义保形
    async fn spawn_bg(&self, ws: &WsId, spec: ExecSpec) -> Result<ProcId>;
    async fn poll(&self, ws: &WsId, id: &ProcId) -> Result<ProcState>;
    async fn kill(&self, ws: &WsId, id: &ProcId) -> Result<()>;
    fn lifecycle(&self, ws: &WsId) -> WsHandle;                                 // create/ttl/dispose
}
```

实现：`Local`（过渡期）/ `DockerSsh`（ARM）。backend 对接的单元是**语义不是命令**：file_edit 失败时模型拿到的仍是结构化 not-found 错误，不是一段 stderr。

### 5.2 工位实例

- **一种标准镜像**（git/gh/常用运行时），**ttl 参数化**：即焚（任务容器，跑完 `--rm`）/ 常驻（登录态、配置积累——"攒出来的电脑"）。容器数量是任务涌现的结果，不是架构预设分类。
- 实例内 agent 的 shell/file 工具是容器内本地调用（不经 backend）——主 agent 的 file 工具保持身体域，工位内文件活由实例自己的工具面完成。

### 5.3 宿主 launcher（治理执行点）

ARM 上薄服务：镜像白名单、实例并发配额、key 发放/回收记录、实例注册表。**主 daemon 保持零繁殖逻辑**——身体不管人口，边境管入境。繁殖权（哪些任务允许工位档、上限）归 operator 档位。

### 5.4 凭证三模式

| 模式 | 适用 | 说明 |
|---|---|---|
| 经主 daemon 代理 | 即焚短任务实例 | 免发 key 管理成本 |
| 独立 key 分配 | 常驻/重活实例 | 吞吐隔离、手术免疫（主 daemon 热切换不中断实例推理）、配额归因到实例（对照 GLM 1302/1308 教训） |
| 共享主凭证下发 | —— | **排除**：爆炸半径扩大且无法归责 |

发 key/收 key 归元操作（operator 让渡面）。ARM 既有 cli-proxy-api 为潜在落点。

## 6. 委托模型：delegate 退役

### 6.1 吸收路线

多实例架构下 delegate 的每个机制都有一等原语新家：

| delegate 机制 | 新家 |
|---|---|
| sync 阻塞返回 | `myclaw exec --agent X "任务"`（shell 阻塞，stdout 即结果；exec 入口已存在） |
| async 唤醒注入 | 任务 prompt 指示完成后 `send_message` 回主 gateway（agent 间消息+唤醒链已端到端验证）。回流逻辑是 prompt 编程，非协议硬编码 |
| 超时/resume | 常驻实例：会话即消息流，再发一条"继续"即 resume；即焚实例：崩了重跑（牲口不抢救） |
| 工具白名单 | agents 配置面（exec `--agent` 挑选） |
| worktree 隔离 | 容器本身（隔离从目录级升物理级，worktree 成为容器内普通 git 操作） |
| 治理 | 宿主 launcher（§5.3） |

**统一律**（本 RFC 第三次出现）：能力获得一等原语（进程身份/语言/协议）后，专用封装从机制退化为糖衣——ssh 吸收多机寻址、消息吸收委托、shell 吸收工具组合。

### 6.2 inline 档（提案，待批）

delegate 收敛后保留两档，**选择器是任务的目标域而非任务大小**：

- **inline = 身体域子任务档**：整理记忆、skill 提议、会话分析——目标对象在身体里，实例化要求跨消息协议操作记忆，荒谬（skill_proposer 为现成例子）。工具面天然仅身体工具。
- **工位档 = 世界域子任务档**：写码/clone/分析——目标对象在世界，必须进容器。

### 6.3 防错配套

模型自拼 CLI 出错面大于 schema 校验（忘 `--user`、寻址拼错、回流指示写漏 → 结果黑洞）。配套：`exec --user` 必选（已有，fail-fast）+ 组合知识沉淀为 skill（"派活给工位实例"含回流模板）——签名层留给治理与契约，进程层交给语言，组合知识归 skill。

### 6.4 sessions_yield 悬停契约（契约 W endgame，issue #256）

**决议（2026-09-05，决策人）**：契约 W 记为本 RFC 原则区 endgame（§1.7）；现状契约 Y 为过渡实现。配套条款：①挂起态唯一权威源=持久化 history（内存 waiter 仅为加速结构）；②契约 W 的重放幂等义务——任何执行可能跨越重启的工具，重跑必须无害。

**已落地（同任务 park 变体，过渡）**：Phase 1/2a/2b（#257/#258/#259）——sessions_yield 的回合在 `run_and_deliver::park_for_yield` 物理 park（双锁穿线 + 单写者 history）；execute() 本身照常返回 stub，`EndTurn` 信号在 SessionContext 层被转译为 park。外部语义与契约 W 一致（无磁盘 resume、history 无 stub、单写者），机制上保留 Y 的 EndTurn 骨架。#262 补齐委托子会话的 notice→park 路由（registry miss 不再生成幽灵实例，通知经 try_fill 进入 park waiter）。三条交付路径（fast path / live-wake / 插话取消）已生产实证（C/D park 115.5s / 295.5s）。**路线**：2b 为过渡；endgame 终态 = 等待点移入工具帧 + 跨重启重放（见下方未竟清单），届时 §1.7"工具自身写入"完全成立。

**未竟（open）**：
- 缺陷① stub 流渲染（近期待办①，契约无关）：`tool_phase.rs` 对 deferred yield 仍无条件发 `TurnEvent::ToolResult` + `End{success}`，客户端把悬停渲染成完成
- 重启重放（惰性/急切）未实现；幸存不变项（`identify_breakpoint` / sole-orphan 门 / #242 持久化）已就位
- 溶解清单未动（~150 行特判）：`is_deferred_yield`、`try_fill_pending_yield`、`resume_after_yield`、`insert_implicit_yield`、stub emit——注意 #262 的 park 路由当前借道 try_fill，溶解时改为直呼 waiter
- park 无独立超时（2b 备注一）；#255 过期事件积压

**与 D8 的关系**：契约 W 是会话原语——sessions_yield 的事件源语义独立于 delegate 工具族，delegate 退役（D8，待批）不撤销本条；退役后 sessions_yield 的归宿随实例会话模型走。

## 7. 自演进与治理

### 7.1 演进栈与自主档位

| 层 | 对象 | 生效 | 档位（operator 可调） |
|---|---|---|---|
| L1 | 记忆 | 即时 | 自由写 / 蒸馏审 |
| L2 | 技能 | 热重载 | 自由 / 提议+签名 promote |
| L3 | 行为面（AGENT.md/白名单） | 重载 | 自由 / PR |
| L4 | 代码 | 冷（二进制） | 全人审 / auto-merge+抽样审 / 全自动+e2e 守门 |

验证强度是**策略不是结构**；但见 §7.2。

### 7.2 宪法条款（提案，待批）

**自主是 operator 的让渡，不是 agent 的获取**。让渡面本身（授权配置、CI、operator 审批）不可进入 agent 可自改范围——agent 可提议提档，operator 单边裁决（与 skill_proposer"发现权在生命体，执行权在 operator"同构）。若此条降为策略，L4 全自动档等价于 agent 自发权限。

### 7.3 L4 部署闭环

```
工位开发 → PR → CI: build + smoke canary（起容器跑新二进制：
daemon 起/工具注册齐/memory 加载通 → 绿才发 artifact）
→ operator 审（按档位）→ merge → daemon_update（rehearsed 门）：
download → rename swap（.old 留存）→ SIGUSR1 → exit
→ 新版启动自检写 journal → e2e 验收
兜底：systemd ExecStartPre 连续失败 N 次自动回 .old
```

现状缺口（本闭环补齐）：`.old` 留存但无自动回退；canary 排练位进 CI 而非本地影子 daemon（不占 954MB，不涉 qemu 模拟保真度问题）。

### 7.4 让渡面清单（operator 档位）

- 繁殖权：工位档任务范围、实例并发上限、镜像白名单
- 凭证：key 发放/回收（§5.4）
- 自主档位：L1-L4 各档（§7.1）
- 授权变更：提议权归 agent，裁决权单边归 operator，全程审计可见

## 8. 实施阶段

| 阶段 | 内容 | 验收 |
|---|---|---|
| P0 接缝 | body 守卫（含 symlink 用例单测）+ shell 工具改走 backend 接口（Local 唯一实现，行为不变）+ 遗留人格文件清理（git 收档后删，§4.4） | 全工具回归绿；守卫单测过；遗留文件删除且 git 历史可查 |
| P1 工位上线 | DockerSshBackend + ARM launcher + 标准镜像 + 契约保形测试（远端 30K 截断/超时/后台/.out 逐项对齐本地语义） | 一次真实重活全程工位完成 |
| P2 元层补全 | daemon_update（rehearsed+handoff+自动回退）/ status / logs；诊断 skill 族改走自省工具；CI smoke canary | 一次真实部署不经宿主 shell |
| P3 深化 | coder 工位实例化 + task 统一体（cronjob 收编）+ delegate 退役（exec/消息路径打通）+ 常驻实例与独立 key | 一次委托任务走实例路径完成 |
| P4 收口 | 本地 body-shell sunset。**前置**：部署/诊断/迁移三类职责全迁完 | daemon 宿主上 agent 无任何字节通道；autonomy 全按签名白名单 |

顺序硬约束：P4 必须最后、显式、单独做。中间任何一步停下，系统比现状多复杂度而未获隔离收益。

## 9. 开放问题（决策人批注点）

- **Q1** §7.2 宪法条款强度：守门人不可自改是**结构**（不可配置）还是**策略**（operator 可放开）？
- **Q2** delegate 是否完全退役（§6.1），还是保留薄封装工具（spawn+发任务糖衣）？
- **Q3** 若完全退役：`agent_delegate`/`agent_resume`/`agent_kill` 工具族的兼容窗口与下线节奏？
- **Q4** §5.4 即焚实例默认走主 daemon 代理是否成立？（代理模式将主 daemon 放入推理数据路径，与手术隔离原则有张力，仅在即焚档主张可接受）
- **Q5** §6.2 inline 档存废：域选择器论证是否成立，还是全部子任务实例化？
- **Q6** ~~人格文件归属~~ **已解决（2026-09-01）**：查证为废弃遗留（运行时零引用，语义已迁 `agents/main/AGENT.md` 与 `user_profile`），撤销裁决，改列 P0 清理项（§4.4）。编号保留以稳定后续引用。
- **Q7** §D12 消息默认中转（经主 gateway）vs P2P 直连？
- **Q8** L4 自主档位上限：是否提供全自动档，还是最高 auto-merge+抽样审？
- **Q9** 过渡期双 shell（本地+工位）的治理裂缝窗口如何监控（期间 body 守卫为半开大门）？

## 10. 备选方案与拒绝理由

1. **userns 轻工位**：只做视图转换不做边界（共享内核、无资源限额、网络另配）；坑为持续运维税（root 投影 nobody/sudo 假象/属主错乱，已付学费）；工位语义要求可丢弃负载，userns 在共享文件系统上划格子，清理责任仍在。拒绝。
2. **本地 Docker**：954MB 内存不可行；且不解决身体/工位物理隔离。拒绝（换机后经 backend trait 可平迁回）。
3. **按角色预设容器分类**（task/role/coder 三类）：真实差异仅"谁在里面干活/活多久"两维度，多实例统一后被参数坍缩。拒绝（涌现优于预设）。
4. **凭证恒经主 daemon 代理**：主 daemon 进入推理数据路径，热切换瞬间中断全部实例推理——违反第一原则。修正为三模式。
5. **update 裸 shell 长期保留**：竞品实证全部走专用通道+事务语义；`cmd_update.rs` 已是事实专用事务，缺签名层与 rehearsed 门。修正为元工具。
6. **delegate 工具长期保留**：单机时代三基础件（进程身份/CLI 入口/消息协议）皆缺故必须专用机制；实例化后齐备，保留为重复心智模型。提案退役（待 Q2/Q3）。

---
*本 RFC 为 2026-08-31 会话收敛稿，2026-09-01 首次修订（Q6 解决）。§9 现余八个待批注分歧点（Q6 已解决，编号保留）；§2 状态列区分"会话收敛"与"提案待批"。修订记录见元信息块。*
