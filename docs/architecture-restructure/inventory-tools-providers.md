# 模块盘点 B：src/tools/ + src/providers/

仓库：/home/ubuntu/.myclaw/workspace/MyClaw（只读）。所有数据为实测（wc -l / grep），非估算。
总计：tools 33 文件 16784 行；providers 42 文件 10321 行。

## 1. 职责一句话 + 名实判断

- **src/tools/**：内置工具（shell/文件/搜索/任务/记忆/cron/多代理委托/音视频等）的具体实现，全部 `impl crate::providers::Tool`。**名实基本相符**，但注意：ToolRegistry（工具注册表）不在 tools 里，而在 `src/agents/tool_registry.rs`——发现/注册逻辑分散在 agents + daemon。
- **src/providers/**：LLM 厂商接入层 + capability trait 家族（Chat/Embedding/Tool/Search/STT/TTS/Image/Video）。**名实不符一处**：核心域 trait `Tool/ToolResult/ToolSource/ToolSpec`（capability_tool.rs）定义在 providers——它是 tools 模块依赖的"域概念"，放在基础设施命名的模块里，且该 trait 反向依赖 `crate::agents::session::Session`（见 §5）。

## 2. 全文件清单（75 文件：33 tools + 42 providers）

pub 数 = `pub fn/struct/enum/trait/type/const/use` 计数（grep 实测）。

### src/tools/（33 文件 16784 行）
| 路径 | 行数 | 职责 | pub数 |
|---|---|---|---|
| mod.rs | 129 | 模块声明/重导出 + builtin_tools()（无状态工具装配） | 34 |
| shell.rs | 2012 | ShellTool/ShellPollTool/ShellKillTool + 进程登记表 ProcEntry 持久化/重启收养/reaper | 22 |
| cronjob_tool.rs | 1584 | CronJobTool：cron 任务 CRUD（参数解析/delivery/webhook 校验全在此） | 2 |
| memory_tool.rs | 1406 | Memory List/View/Search/Manage 四工具 + frontmatter lint/PII 扫描/审计 | 8 |
| task.rs | 1005 | TaskCreate/List/Update/Delete + 每会话 TaskBoards（tasks.json） | 16 |
| send_message.rs | 968 | SendMessageTool：跨用户/代理间消息（走 channels + 好友裁决） | 6 |
| file_ops.rs | 939 | FileRead/Write/Edit（部分读写/符号定位） | 6 |
| skill_manage_tool.rs | 882 | SkillManageTool：skill CRUD + workspace/skill_loader 校验 | 2 |
| search.rs | 675 | GlobSearchTool + ContentSearchTool | 4 |
| memory_tool_tests.rs | 585 | memory 工具测试（cfg(test) 独立文件） | 0 |
| friends.rs | 550 | Friend Request/Accept/Decline/List 四工具 + FriendToolsCtx | 12 |
| view_video.rs | 421 | ViewVideoTool | 2 |
| view_image.rs | 414 | ViewImageTool | 2 |
| hear_audio.rs | 411 | HearAudioTool（STT） | 2 |
| skill_tool.rs | 408 | SkillTool：按需加载 skill 正文 | 2 |
| search_cooldown.rs | 404 | SearchProviderCooldown（搜索降级/冷却状态机） | 9 |
| shell_env.rs | 373 | shell 环境构造（PATH、禁用别名等） | 2 |
| calculator.rs | 372 | CalculatorTool（表达式求值） | 2 |
| web_search.rs | 322 | WebSearchTool | 2 |
| delegate.rs | 293 | AgentDelegateTool：子代理委托 | 2 |
| list_dir.rs | 291 | ListDirTool | 2 |
| http.rs | 285 | HttpRequestTool | 2 |
| symbol_check.rs | 277 | SymbolCheckTool（代码符号定位） | 2 |
| session_query.rs | 269 | SessionQueryTool（读 SessionBackend 历史） | 2 |
| tool_search.rs | 260 | ToolSearchTool（查 ToolRegistry） | 2 |
| skills_list_tool.rs | 222 | SkillsListTool | 2 |
| agent_resume.rs | 178 | AgentResumeTool（续跑超时子代理，读 DelegationCheckpoint） | 2 |
| ask_user.rs | 176 | AskUserTool（AskRouter 等人回复） | 2 |
| media_download.rs | 172 | 音视频下载辅助 | 2 |
| agent_kill.rs | 164 | AgentKillTool | 2 |
| truncation.rs | 140 | truncate_output/truncate_tool_result（框架级输出截断） | 3 |
| sessions_yield.rs | 123 | SessionsYieldTool | 2 |
| agent_list.rs | 74 | AgentListTool | 2 |

### src/providers/（42 文件 10321 行）
| 路径 | 行数 | 职责 | pub数 |
|---|---|---|---|
| mod.rs | 89 | 声明 + 大量 re-export | 55 |
| error_class.rs | 1074 | 错误三分类（HTTP/厂商业务码/兜底）+ 恢复提示 | 20 |
| media.rs | 960 | 按模型降级富媒体（MediaPolicy/marker/lowering） | 37 |
| protocols/openai/chat_completions.rs | 787 | OpenAI ChatCompletions 客户端+SSE | 5 |
| fallback.rs | 689 | FallbackChatProvider 装饰器（链式故障转移） | 6 |
| protocols/openai/responses.rs | 537 | OpenAI Responses API 客户端 | 6 |
| capability_chat.rs | 480 | ChatProvider trait + ChatMessage/StreamEvent 等 | 27 |
| protocols/google/message_rendering.rs | 453 | Google 请求体渲染 | 1 |
| protocols/google/generate_content.rs | 424 | Google GenerateContent 客户端 | 3 |
| protocols/anthropic/messages.rs | 401 | Anthropic Messages 客户端 | 4 |
| provider_factory.rs | 387 | 按 (provider_id, protocol) 构造一切 provider | 23 |
| protocols/openai/chat_message_rendering.rs | 376 | OpenAI 请求体渲染 | 1 |
| credential_pool.rs | 354 | 同厂商多凭证轮换池 | 23 |
| glm_mcp.rs | 323 | GLM Coding Plan MCP 搜索包装 | 3 |
| protocols/anthropic/message_rendering.rs | 302 | Anthropic 请求体渲染 | 3 |
| google.rs | 284 | GoogleProvider（chat+embedding 包装） | 3 |
| openai.rs | 277 | OpenAiProvider（chat+embedding+image 包装） | 4 |
| glm.rs | 265 | GLM：embedding+search 实现 + glm_body_override | 5 |
| protocols/openai/responses_rendering.rs | 243 | Responses 请求体渲染 | 1 |
| xiaomi.rs | 189 | XiaomiProvider（Anthropic/OpenAI 双协议） | 5 |
| shared.rs | 181 | AuthStyle + 流式 UTF-8 解码 | 6 |
| capability.rs | 156 | Capability/Modality/模型配置定价 | 12 |
| minimax.rs | 152 | MiniMaxProvider | 4 |
| provider_registry.rs | 123 | ProviderRegistry trait（路由入口） | 4 |
| provider_id.rs | 114 | ProviderId + well_known + URL 探测 | 16 |
| capability_tool.rs | 105 | **Tool/ToolResult/ToolSpec/ToolSource 域 trait** | 4 |
| edge_tts.rs | 100 | Edge TTS（子进程免费 TTS） | 3 |
| kimi.rs | 56 | KimiProvider（委托 OpenAI 客户端） | 4 |
| deepseek.rs | 56 | 仅 deepseek_body_override 函数 | 1 |
| qwen.rs | 54 | 仅 qwen_body_override 函数 | 1 |
| anthropic.rs | 53 | AnthropicProvider（53 行纯转发） | 4 |
| image.rs | 52 | ImageGenerationProvider trait | 9 |
| video.rs | 45 | VideoGenerationProvider trait | 8 |
| tts.rs | 43 | TtsProvider trait | 8 |
| stt.rs | 39 | SttProvider trait | 7 |
| capability_embedding.rs | 31 | EmbeddingProvider trait | 7 |
| search.rs | 30 | SearchProvider trait | 4 |
| http.rs | 20 | build_reqwest_client | 1 |
| protocols/mod.rs | 9 | 协议层声明 | 3 |
| protocols/openai/mod.rs | 4 | 声明 | 4 |
| protocols/google/mod.rs | 2 | 声明 | 2 |
| protocols/anthropic/mod.rs | 2 | 声明 | 2 |

## 3. providers 碎片化评估（7 簇）

| 簇 | 文件数 | 行数 | 文件 |
|---|---|---|---|
| 协议客户端层 protocols/ | 12 | 3540 | openai 5 文件 1947 行（含 mod）、google 3 文件 879、anthropic 3 文件 705、protocols/mod 9 |
| 基础设施（factory/错误/凭证/降级/共享） | 7 | 2819 | error_class 1074、fallback 689、credential_pool 354、provider_factory 387、provider_id 114、shared 181、http 20 |
| 厂商包装层 | 9 | 1386 | google 284、openai 277、glm 265、xiaomi 189、minimax 152、anthropic 53、kimi 56、deepseek 56、qwen 54 |
| capability trait 层 | 5 | 895 | capability 156、capability_chat 480、capability_embedding 31、capability_tool 105、provider_registry 123 |
| media | 1 | 960 | media.rs |
| 搜索 | 2 | 353 | search.rs 30、glm_mcp.rs 323 |
| 其他模态实现 | 5 | 279 | edge_tts 100、image 52、video 45、tts 43、stt 39 |
| mod.rs | 1 | 89 | — |
| 合计 | 42 | 10321 | ✓ |

**该合并的（强互引证据）**：
1. **微厂商文件 → 并入厂商簇或 protocols**：anthropic.rs（53 行，整个文件就是 `chat()` 转发到 `protocols::anthropic::messages::AnthropicMessagesClient`）；deepseek.rs/qwen.rs（各仅 1 个 `*_body_override` 函数，唯一调用方是 provider_factory.rs:163/173）；kimi.rs（56 行，委托 OpenAI 客户端）。4 文件 219 行可合成一个 `vendor_overrides.rs` 或并入 factory。互引证据：provider_factory.rs:153/163/173/216 直接 import 这四个文件 + protocols 客户端；anthropic.rs/kimi.rs/xiaomi.rs 反向 import protocols::（各 1-3 处）。
2. **shared.rs + http.rs（201 行）**：同为"厂商共享工具"（AuthStyle/UTF-8 解码 vs reqwest Client 构造），protocols 客户端两者都用——合并为 `infra.rs`。
3. **微 capability trait 文件**：capability_embedding 31 / image 52 / video 45 / tts 43 / stt 39 共 210 行 5 文件，均为单 trait + 少量 DTO，可合为 `capability_media.rs`（与 media.rs 960 行就近）。
4. **search.rs(30) + glm_mcp.rs(323)**：trait 与唯一厂商实现，glm_mcp 被 factory:332 引用，可同目录聚合（不必须合并）。
不建议合并：protocols 三家内部已按 命名规整；error_class 1074 行偏大但内聚（分类管线）。

## 4. tools→agents 依赖符号级明细（实测 29 文件引用，非 19）

**分类 A：仅 `_session: &crate::agents::session::Session` 形参（Tool trait 签名强制，函数体不用）** —— 8 文件，倒置成本≈0，改 trait 签名即消除：
calculator.rs:53、http.rs:132、symbol_check.rs:158、web_search.rs:62、search.rs:155+462、sessions_yield.rs:69、agent_list.rs:46（另有 use DelegationCoordinator，见 D）、cronjob_tool.rs:179（另有 scheduler，见 C）

**分类 B：用 Session 实体（读 owner/身份等）**：
- shell.rs（execute 三处签名 902/1093/1226 + set_session_manager 注入 SessionManager，见 E）
- task.rs:300/434/513/603（session 定位 TaskBoard）
- ask_user.rs:67（Session::resolve_channel + AskRouter）
- send_message.rs:10（use Session）、memory_tool.rs:17、session_query.rs:11、view_video.rs:8、view_image.rs:7、hear_audio.rs:8、file_ops.rs:118/520/624、skill_tool.rs:59、list_dir.rs:141、skills_list_tool.rs:45、agent_resume.rs:103、agent_kill.rs:91、delegate.rs:129/236、skill_manage_tool.rs:104、memory_tool_tests.rs:9、friends.rs:23

**分类 C：引调度/编排实体**：
- cronjob_tool.rs:8-12 `use crate::agents::{SharedScheduler}` + `use crate::agents::scheduling::cron_types::{DeliveryConfig,DeliveryMode,FailureAlertConfig,RetryConfig,ScheduleKind,ScheduleSpec}` + `use crate::agents::scheduling::scheduler::{self,JobEntry,validate_active_hours,validate_at_timestamp,validate_schedule,validate_tz}`（+943 RunRecord）——**最重依赖**，scheduler 类型系统整体暴露给工具层
- delegate.rs:17 `use crate::agents::AgentDelegator` + :123/:260 `crate::agents::SUB_AGENT_TIMEOUT_MAX_SECS`
- agent_resume.rs:11 `DelegationCoordinator`
- ask_user.rs:14 `ask_router::AskRouter`

**分类 D：引注册表/管理器**：
- tool_search.rs:10+130 `use crate::agents::ToolRegistry`（对 ToolRegistry 做 self-reflection 查询）
- skill_tool.rs:10、skills_list_tool.rs:10 `use crate::agents::SkillManager`
- skill_manage_tool.rs:10-11 `use crate::agents::workspace::skill_loader` + `use crate::agents::{Skill, SkillManager}`
- agent_kill.rs:6 `use crate::agents::{DelegationCoordinator, RunningAgentInfo}`（+125 DelegationStatus）
- agent_list.rs:6 `DelegationCoordinator`
- shell.rs:704/746 `OnceLock<Arc<crate::agents::SessionManager>>`

**分类 E：引用户/社交/消息域**：
- send_message.rs:11-12 `use crate::agents::{AgentMail,AgentMessage,AgentMessenger,KnownUsersRegistry,MessageKind,UserRegistry}` + :267 `commands::register::parse_target` + :286-313 `DeliveryVerdict::{Allowed,Blocked,NotFriends}` + `UserMail` + :803 `UserResolver::new`
- friends.rs:21-24 `commands::friends::rk_for`、`commands::register::parse_target`、`{ContactStatus,KnownUsersRegistry,RequestOutcome,UserMail,UserRegistry}` + :36 `OnceLock<crate::agents::ChannelRegistry>` + :105 `ContactEntry` + :525 `ContactDirection`
- session_query.rs:12 / memory_tool.rs:18 `user_profile::UserResolver`

**修复方向结论**：A 类纯签名问题（改 Tool trait 的 session 参数为窄接口即可）；C/D 类是"工具直接摸 agents 内部件"（scheduler/SkillManager/DelegationCoordinator/ToolRegistry 应下沉为独立模块或经 trait 注入）；E 类说明 send_message/friends 本质是社交域工具，依赖的是 agents 里的 user/social 子系统——该子系统应从 agents 拆出为独立模块，而非 tools 改造。

## 5. providers→agents 反向边专查（实测 5 文件，不是 1 个）

1. **capability_tool.rs:6** `use crate::agents::session::Session;` —— Tool trait 的 `execute(&self, args, session: &Session)` 签名（:96-99）持有 Session 全类型。原因（doc 注释实证）：per-user 工具（memory_*、ask_user）读 `session.owner` 做 scope。**能否 trait 反转**：能且应该——把 execute 需要的字段收窄成 `ToolContext { owner, session_id, reply_target, last_message }` 值对象，放 providers（或独立 domain 模块），agents 反向构造它。这是依赖倒置修复的**第一杠杆点**：一次改动同时消掉 tools 29 个文件里 A/B 两类 Session 引用的根。
2. **protocols/anthropic/messages.rs:86,100,106,116**、**google/generate_content.rs:80,94,100,110**、**openai/chat_completions.rs:95,109,115,125**、**openai/responses.rs:102,116,122,132** —— 四文件代码块雷同（同一超时模式复制 4 份），只引 `crate::agents::llm_stream::{REQUEST_SEND_TIMEOUT, ERROR_BODY_TIMEOUT}` 两个 `Duration` 常量。**反转成本≈0**：把 2 个常量搬到 providers（如 shared.rs/http.rs），providers→agents 边即归零（除 capability_tool 的 Session）。同时顺手把 4 处重复超时代码提成共享 helper（一份 ~40 行的发送+超时包装）。

## 6. tools 功能簇与三大文件剖面

**注册结构**：`tools/mod.rs::builtin_tools()` 只装配无状态核心（shell×3、file×3、search×2、symbol_check、http、calculator）；**有状态工具全部由 `src/daemon.rs::build_tools()`（:491-690）注册**：ask_user、send_message、list_dir、task 系列（TaskBoards）、skill×3（SkillManager）、cronjob（SharedScheduler）、memory×4（UserResolver）、session_query（SessionBackend）、friends×4（FriendToolsCtx/注册表）、MCP 工具（McpManager 注入）。发现机制：ToolRegistry 定义于 `src/agents/tool_registry.rs:13`，daemon 构造后注入 Agent（agent.rs:1896）；tool_search 工具对同一 registry 做查询。
- **shell.rs 2012**：ProcEntry 持久化登记表（磁盘 JSON + /proc liveness 探测 pid_start_ticks）→ owned/adopted reaper 后台收割 → adopt_after_restart 热切换收养（:530）→ ShellTool/Poll/Kill 三实现（:691/1043/1190）→ :1286 起全为 tests（约 726 行测试）。top 函数（按体量）：set_session_manager 109 行、adopt_after_restart 77、spawn_adopted_reaper 39、format_unknown_process_listing 36、latest_entry_summary 35、spawn_owned_reaper 34。
- **cronjob_tool.rs 1584**：单 tool + 参数解析全家桶。top：parse_webhook_channel 89、parse_delivery_object 32、format_unknown_job_listing 26、parse_duration_to_ms 23、parse_schedule_input 21。本质是 agents::scheduling 的"参数解析 + 格式化"适配层——若 scheduling 拆出，此文件可跟随。
- **memory_tool.rs 1406**：4 工具 + lint/PII/审计/评分。top：lint_memory_content 59、build_frontmatter 58、best_snippet 50、append_memory_audit 42、scan_agent_pii 36、archive_memory_version 32。唯一引 `crate::memory`（MemoryFile）的 tools 文件。

## 7. 依赖方向明细（tools/providers → 其他模块）

- **crate::channels（3 文件）**：send_message.rs:14（ChannelFile,ChannelFileMeta,ChannelMessageContent,ChannelOutboundMessage,LocalFileBody,MessageReceiver,SendOptions）、friends.rs:25（ChannelMessageContent,ChannelOutboundMessage,MessageReceiver）、ask_user.rs:15（同前三）。tools 依赖 channels 属合理方向（工具即渠道出口）。
- **crate::storage（2 文件）**：agent_resume.rs:13（DelegationCheckpoint）、session_query.rs:14（SessionBackend,SessionInfo）。
- **crate::config（13 文件）**：providers/media.rs×9 处（provider::Protocol 四处 match）、provider_factory.rs:7（Protocol）、tools/cronjob_tool.rs×4（scheduler::ContextPolicy）、file_ops.rs×4、search.rs×2、list_dir.rs×2、hear_audio/memory_tool/shell_env/view_image/view_video 各 1。均为读配置枚举，方向正常。
- **crate::mcp：0 处**（tools/providers 均不直接引；MCP 工具经 daemon 注入）。
- 另：tools→providers 33 文件全部（Tool/ToolResult 是实现契约）；tools→memory 仅 memory_tool.rs。

## 8. 异味清单（文件:行号）

1. providers/capability_tool.rs:6 — 域 trait `Tool` 反向 use agents::session::Session，层间循环根源（agents 也 use providers::Tool）。
2. providers/protocols/{anthropic/messages.rs:86, google/generate_content.rs:80, openai/chat_completions.rs:95, openai/responses.rs:102} — 引 agents::llm_stream 两个常量；且同段超时样板复制 4 份（各 ~40 行）。
3. providers/mod.rs:88 — `pub use reqwest::Client;` 三方类型裸 re-export，污染 API。
4. tools/mod.rs:6 + agents/tool_registry.rs:13 — ToolRegistry 放 agents、Tool trait 放 providers、实现放 tools：同一概念三处分布。
5. tools/shell.rs:704,746 — `OnceLock<Arc<SessionManager>>` setter 注入（issue #140 注释自认是构造顺序权宜）。
6. tools/friends.rs:36,64 — `OnceLock<ChannelRegistry>` setter 注入，同类问题。
7. tools/send_message.rs:267-313 — 直接用 agents::commands::register::parse_target + DeliveryVerdict 三态匹配，社交裁决逻辑内联在工具里。
8. tools/cronjob_tool.rs:8-12 — import agents::scheduling 6 类型 + 5 校验函数 + scheduler self，跨模块白盒耦合（1584 行里近半是 agents 类型的解析/校验）。
9. providers/google.rs:78,96,98,104；protocols/openai/chat_completions.rs:409,419 — 多处 `#[allow(dead_code)]` 字段。
10. providers 碎片化：anthropic.rs(53)/deepseek.rs(56)/qwen.rs(54)/kimi.rs(56) 四个 ≤56 行文件、capability_embedding(31)/stt(39)/tts(43)/video(45)/image(52) 五个微 trait 文件、shared.rs+http.rs 双"共享工具"文件（§3）。
11. providers/error_class.rs — 1074 行单文件承载分类+格式化+恢复策略，是 providers 最大文件，拆分候选。
12. 测试内嵌：tools 几乎每文件尾部 mod tests（shell.rs 测试占 726/2012 行；skill_manage_tool.rs 882 行中约 290 行测试），仅 memory_tool 有独立测试文件——风格不一。
13. daemon.rs:491-690 — build_tools 15+ 参数、串联注册 30+ 工具，工具装配知识集中在 daemon（组合根，可接受但已是异味边缘）。
