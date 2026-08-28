# RFC: Channel 职责拆分 — turn-channel 双重职责的解构

- **状态**: 已完成（实施落地 2026-08-2x；P4 前置核实验象：resolve 链/scheduled §1.1/毒化移除均在线；本字段 2026-08-28 补更）
- **日期**: 2026-08-17
- **背景 bug**: cron Inject 模式输出不投递（用户周五 10 点未收到周刊，2026-08-14 实锤）

## 0. 问题陈述

`process_turn(inbound, channel: Option<Arc<dyn Channel>>, runtime)` 的 channel 参数承载了两个
正交职责：

1. **标记**：「这一轮有没有对话对端」（headless 与否）→ 控制 `ask_user` / `send_message`
   工具可见性
2. **句柄**：「往哪发」→ streaming 折叠、fallback send、TTS、工具进度事件

耦合导致的设计缺陷：

- **cron Inject 输出不投递**（ffa0317 引入 `should_send = !is_active`）：Inject 注入用户
  session 后，因 session active 而跳过 `send_to_target_internal`；而 `run_scheduled_turn`
  传 `channel=None`，turn 内也零投递。两条路都堵死 → 输出只存在于 session history。
- **session 级 run_mode 毒化**：Inject cron 复用用户 session 时把
  `session_override.run_mode = Background` 持久写入（scheduled.rs），用户后续交互轮的
  prompt 交互规则段（prompt.rs:158）被永久切走。
- **headless 轮 send_message 被连坐禁用**：`session.channel = None` → 工具可见性过滤
  （agent.rs:1136）一刀切剔除，而 headless 轮发中间通知本是合理能力。

## 1. 设计

### 1.1 职责「标记」→ 消息携带 run_mode

`RunMode::{Interactive, Background}`（config/agent.rs）已存在，语义即「is there a human
user present?」。改动其作用域：

- `ChannelInboundMessage` 新增 `run_mode: RunMode` 字段（serde default = Interactive，
  与 `silenced_override` 同模式，向后兼容）
- 赋值点：
  - 用户消息 / daemon 恢复合成消息 / 委派 wake → default（Interactive）
  - cron / heartbeat 合成消息（scheduled.rs）→ 显式 `Background`
- `process_turn` 读消息 → 写 `session.turn_headless: bool`（turn-scoped transient，
  同 `turn_silenced` 模式，轮末清理）→ 同时流入 `prompt_config.run_mode`
  （session_context.rs:756 不再读 override）
- scheduled.rs 删除 `session_override.run_mode = Some(Background)` 的 session 级写入

### 1.2 职责「句柄」→ session 持有 registry，消费点现查

- `Session` 新增 transient 字段（SessionContext 物化时接线，同 `persist` hook 模式）：
  - `channels: Option<Arc<ChannelRegistry>>`
  - `channel_account: Option<(String, String)>`（account key；`_cron_*` /
    `_heartbeat_*` / 子代理 session id → None）
- 新增 `Session::resolve_channel() -> Option<Arc<dyn Channel>>`：registry 现查。
  registry 是运行时构造、配置热更后重建的唯一真源——session 只存 Arc 引用，无陈旧句柄
- 删除 `Session.channel` 字段，消费点全部改 `session.resolve_channel()`：
  - `ask_user`：先查 `turn_headless` → 报「后台轮无用户可提问」；再 resolve_channel
  - `send_message`（非子代理分支）：resolve_channel；可见性过滤（agent.rs:1136）改为
    `resolve_channel().is_some() || parent_session_id.is_some()`
  - `tool_executor.rs:174` 审批路由、`agent.rs:741/849` on_tool_event：resolve_channel

### 1.3 process_turn 参数 → 纯投递语义

签名形状不变（`Option<Arc<dyn Channel>>`），语义收窄为**只管 turn 引擎自身的投递**：
streaming 折叠（:729）、fallback send（:1042）、TTS（:1085）、取消/错误通知。
调用方不变：用户轮 / wake / 恢复传 Some，scheduled 传 None。

- 删除 `session.channel = channel`（:742）及全部清理点（:821 / :908）
- process_turn 文档注释重写：参数是投递句柄，不再影响工具可见性

### 1.4 scheduled.rs 修正

1. 删 ffa0317 的 `should_send = !is_active` 块 —— headless 轮 turn 内零投递，
   `send_to_target_internal` 是唯一出口，无条件调用
2. 删 `session_override.run_mode` 的 session 级写入（1.1 已由消息携带）

## 2. 修复的存量问题

| # | 问题 | 根因 | 修复 |
|---|------|------|------|
| 1 | cron Inject 输出不投递 | ffa0317 `should_send = !is_active` 误判 | §1.4-1 |
| 2 | Inject cron 毒化用户 session run_mode | run_mode 放在 session_override | §1.1 |
| 3 | headless 轮 send_message 连坐禁用 | 标记与句柄耦合于 `session.channel` | §1.2 |

## 3. 明确不做

- 不改 `process_turn` 签名形状（参数保留，语义收窄）
- 不做「turn 纯执行、投递全在调用方」（streaming 需要 turn 内响应流事件，方案 C 否决）
- 不动子代理消息路径（send_message 的 DelegationEvent 分支不变）
- 不动 `filter_turn_scoped_tools` 的 friend-tools main-agent-only 规则

## 4. 实施顺序

1. RFC 落盘（本文档）
2. coder：types.rs → ctx.rs/manager 接线 → session_context.rs → 工具层 → scheduled.rs
   → recovery.rs（:422-452 手工 set `session.channel` 的恢复路径改 resolve）→ 测试
3. CI（x86 Micro 禁本地编译）+ `myclaw update` 部署
4. 验证：cron Inject 投递恢复（下周五 10:00 观察）+ 用户轮 ask_user / streaming /
   send_message 无回归

## 5. 测试要点

- cron Inject 轮：turn_headless=true、send_message 可见（resolve 到 wechat）、
  ask_user 报错不挂起、`send_to_target_internal` 无条件投递
- 用户轮：run_mode=Interactive、ask_user / streaming / TTS 正常
- Inject 后用户轮不受毒化（prompt run_mode 恢复 Interactive）
- serde 兼容：旧 jobs.json / 消息反序列化 run_mode 走 default
- 恢复路径：daemon 重启合成消息 → Interactive、send_message 工具保留（cf66d49 不回归）
