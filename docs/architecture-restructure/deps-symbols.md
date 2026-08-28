# 跨模块符号级引用（use 源 + inline 源双源合并；符号=路径首段）


## agents → providers

- `capability_chat`: 24 处 — agents/commands/session.rs:106, agents/context_engine.rs, agents/memory_distill.rs, agents/memory_distill.rs:324, agents/memory_fork.rs, agents/orchestrator/mod.rs, agents/orchestrator/mod.rs:125, agents/orchestrator/recovery.rs:1184
- `StopReason`: 13 处 — agents/agent.rs:2282, agents/agent.rs:2309, agents/agent.rs:2330, agents/agent.rs:2357, agents/agent.rs:2378, agents/session_context.rs, agents/session_context.rs:1013, agents/session_context.rs:58
- `ProviderHttpError`: 12 处 — agents/agent.rs, agents/agent.rs:1686, agents/user_messages.rs, agents/user_messages.rs:192, agents/user_messages.rs:202, agents/user_messages.rs:212, agents/user_messages.rs:223, agents/user_messages.rs:234
- `ContentPart`: 12 处 — agents/memory_distill.rs:618, agents/memory_fork.rs:268, agents/orchestrator/inbound.rs:1087, agents/scheduling/work_unit.rs, agents/session/session_override.rs:272, agents/session/session_override.rs:275, agents/session/session_override.rs:278, agents/session/types.rs
- `ChatMessage`: 11 处 — agents/attachment.rs, agents/commands/info.rs:327, agents/commands/info.rs:330, agents/commands/mod.rs:298, agents/commands/session.rs:124, agents/commands/session.rs:157, agents/commands/session.rs:95, agents/scheduling/work_unit.rs
- `Tool`: 10 处 — agents/agent.rs:1148, agents/agent.rs:1169, agents/agent.rs:1259, agents/mcp_manager.rs:101, agents/mcp_manager.rs:111, agents/mcp_manager.rs:20, agents/mcp_manager.rs:8, agents/mcp_manager.rs:90
- `ToolCall`: 9 处 — agents/agent.rs:1008, agents/orchestrator/mod.rs, agents/scheduling/work_unit.rs, agents/session/session_override.rs, agents/session/types.rs:415, agents/session/types.rs:545, agents/session_context.rs:1551, agents/skill_extract.rs
- `Capability`: 9 处 — agents/commands/config.rs:16, agents/commands/info.rs:144, agents/commands/info.rs:244, agents/commands/info.rs:323, agents/commands/model.rs:16, agents/commands/model.rs:72, agents/commands/session.rs:33, agents/media_e2e_test.rs
- `StreamEvent`: 8 处 — agents/agent.rs, agents/agent.rs:2263, agents/agent.rs:2264, agents/commands/info.rs:351, agents/commands/info.rs:354, agents/commands/info.rs:357, agents/llm_stream.rs, agents/skill_extract.rs
- `(module)`: 7 处 — agents/agent.rs, agents/context_engine.rs, agents/media_e2e_test.rs, agents/memory_distill.rs, agents/memory_fork.rs, agents/orchestrator/test_support.rs, agents/tokens.rs
- `media`: 7 处 — agents/context_engine.rs:1211, agents/context_engine.rs:1722, agents/media_e2e_test.rs:186, agents/media_e2e_test.rs:187, agents/session_context.rs, agents/tokens.rs, agents/tokens.rs:70
- `capability_tool`: 5 处 — agents/memory_distill.rs, agents/memory_fork.rs, agents/skill_extract.rs, agents/tool_executor.rs, agents/tool_registry.rs
- `ThinkingConfig`: 5 处 — agents/orchestrator/turn.rs, agents/session/session_override.rs:62, agents/session/session_override.rs:64, agents/session/session_override.rs:68, agents/turn.rs
- `ClassifiedError`: 4 处 — agents/agent.rs, agents/user_messages.rs, agents/user_messages.rs:27, agents/user_messages.rs:80
- `ProviderRegistry`: 4 处 — agents/commands/mod.rs:44, agents/media_e2e_test.rs, agents/orchestrator/scheduled.rs:350, agents/runtime.rs
- `ChatUsage`: 3 处 — agents/agent.rs:1468, agents/agent.rs:1519, agents/skill_extract.rs
- `MediaPolicy`: 2 处 — agents/agent.rs:1887, agents/orchestrator/test_support.rs:90
- `BoxStream`: 2 处 — agents/llm_stream.rs, agents/skill_extract.rs
- `fallback`: 2 处 — agents/user_messages.rs, agents/user_messages.rs:289
- `ToolSource`: 1 处 — agents/agent.rs:1159
- `ChatRequest`: 1 处 — agents/commands/info.rs:332
- `capability`: 1 处 — agents/media_e2e_test.rs
- `ProviderId`: 1 处 — agents/media_e2e_test.rs:404
- `provider_id`: 1 处 — agents/media_e2e_test.rs:405
- `ChatMessageUsage`: 1 处 — agents/session/types.rs:419
- `TtsRequest`: 1 处 — agents/session_context.rs:1125
- `TtsVoice`: 1 处 — agents/session_context.rs:1128
- `ErrorCategory`: 1 处 — agents/user_messages.rs
- `format_cooldown_zh`: 1 处 — agents/user_messages.rs

## tools → agents

- `session`: 91 处 — tools/agent_kill.rs:91, tools/agent_list.rs:46, tools/agent_resume.rs:103, tools/ask_user.rs:67, tools/calculator.rs:53, tools/cronjob_tool.rs:179, tools/delegate.rs:129, tools/delegate.rs:236
- `DelegationCoordinator`: 3 处 — tools/agent_kill.rs, tools/agent_list.rs, tools/agent_resume.rs
- `ChannelRegistry`: 3 处 — tools/friends.rs:36, tools/friends.rs:64, tools/friends.rs:68
- `user_profile`: 3 处 — tools/memory_tool.rs, tools/memory_tool_tests.rs, tools/session_query.rs
- `DeliveryVerdict`: 3 处 — tools/send_message.rs:286, tools/send_message.rs:306, tools/send_message.rs:313
- `Skill`: 3 处 — tools/skill_manage_tool.rs, tools/skill_tool.rs, tools/skills_list_tool.rs
- `SkillManager`: 3 处 — tools/skill_manage_tool.rs, tools/skill_tool.rs, tools/skills_list_tool.rs
- `(module)`: 2 处 — tools/cronjob_tool.rs, tools/send_message.rs
- `scheduling`: 2 处 — tools/cronjob_tool.rs, tools/cronjob_tool.rs:943
- `SUB_AGENT_TIMEOUT_MAX_SECS`: 2 处 — tools/delegate.rs:123, tools/delegate.rs:260
- `commands`: 2 处 — tools/friends.rs, tools/send_message.rs:267
- `UserMail`: 2 处 — tools/friends.rs, tools/send_message.rs:289
- `SessionManager`: 2 处 — tools/shell.rs:704, tools/shell.rs:746
- `RunningAgentInfo`: 1 处 — tools/agent_kill.rs
- `DelegationStatus`: 1 处 — tools/agent_kill.rs
- `ask_router`: 1 处 — tools/ask_user.rs
- `SharedScheduler`: 1 处 — tools/cronjob_tool.rs
- `AgentDelegator`: 1 处 — tools/delegate.rs
- `ContactStatus`: 1 处 — tools/friends.rs
- `KnownUsersRegistry`: 1 处 — tools/friends.rs
- `RequestOutcome`: 1 处 — tools/friends.rs
- `UserRegistry`: 1 处 — tools/friends.rs
- `ContactEntry`: 1 处 — tools/friends.rs:105
- `ContactDirection`: 1 处 — tools/friends.rs:525
- `UserResolver`: 1 处 — tools/send_message.rs:803
- `workspace`: 1 处 — tools/skill_manage_tool.rs
- `ToolRegistry`: 1 处 — tools/tool_search.rs

## tools → providers

- `Tool`: 30 处 — tools/agent_kill.rs, tools/agent_list.rs, tools/agent_resume.rs, tools/ask_user.rs, tools/calculator.rs, tools/cronjob_tool.rs, tools/delegate.rs, tools/file_ops.rs
- `ToolResult`: 27 处 — tools/agent_kill.rs, tools/agent_list.rs, tools/agent_resume.rs, tools/ask_user.rs, tools/calculator.rs, tools/cronjob_tool.rs, tools/delegate.rs, tools/file_ops.rs
- `provider_registry`: 9 处 — tools/hear_audio.rs, tools/hear_audio.rs:378, tools/hear_audio.rs:385, tools/view_image.rs, tools/view_image.rs:381, tools/view_image.rs:388, tools/view_video.rs, tools/view_video.rs:388
- `Capability`: 9 处 — tools/hear_audio.rs:370, tools/hear_audio.rs:371, tools/hear_audio.rs:372, tools/view_image.rs:373, tools/view_image.rs:374, tools/view_image.rs:375, tools/view_video.rs:380, tools/view_video.rs:381
- `capability`: 6 处 — tools/hear_audio.rs, tools/hear_audio.rs:380, tools/view_image.rs, tools/view_image.rs:383, tools/view_video.rs, tools/view_video.rs:390
- `(module)`: 3 处 — tools/hear_audio.rs, tools/view_image.rs, tools/view_video.rs
- `modality_from_mime`: 3 处 — tools/hear_audio.rs:262, tools/view_image.rs:265, tools/view_video.rs:272
- `FileModality`: 3 处 — tools/hear_audio.rs:263, tools/view_image.rs:266, tools/view_video.rs:273
- `EmbeddingProvider`: 3 处 — tools/hear_audio.rs:373, tools/view_image.rs:376, tools/view_video.rs:383
- `ImageGenerationProvider`: 3 处 — tools/hear_audio.rs:374, tools/view_image.rs:377, tools/view_video.rs:384
- `TtsProvider`: 3 处 — tools/hear_audio.rs:375, tools/view_image.rs:378, tools/view_video.rs:385
- `VideoGenerationProvider`: 3 处 — tools/hear_audio.rs:376, tools/view_image.rs:379, tools/view_video.rs:386
- `SearchProvider`: 3 处 — tools/hear_audio.rs:377, tools/view_image.rs:380, tools/view_video.rs:387
- `SttProvider`: 3 处 — tools/hear_audio.rs:379, tools/view_image.rs:382, tools/view_video.rs:389
- `MediaPolicy`: 3 处 — tools/hear_audio.rs:383, tools/view_image.rs:386, tools/view_video.rs:393
- `media`: 3 处 — tools/media_download.rs:169, tools/media_download.rs:28, tools/send_message.rs:612
- `ClassifiedError`: 2 处 — tools/search_cooldown.rs, tools/web_search.rs
- `capability_chat`: 1 处 — tools/memory_tool.rs:30
- `FailoverReason`: 1 处 — tools/search_cooldown.rs
- `search`: 1 处 — tools/web_search.rs
- `ProviderRegistry`: 1 处 — tools/web_search.rs

## agents → channels

- `MessageReceiver`: 17 处 — agents/agent.rs:1999, agents/ask_router.rs:131, agents/commands/friends.rs, agents/delegation_coordinator.rs:820, agents/orchestrator/delegation.rs:537, agents/orchestrator/delegation.rs:722, agents/orchestrator/delegation.rs:796, agents/orchestrator/recovery.rs
- `ChannelMessageContent`: 16 处 — agents/agent.rs:2000, agents/ask_router.rs:132, agents/commands/friends.rs, agents/delegation_coordinator.rs:821, agents/orchestrator/delegation.rs:544, agents/orchestrator/delegation.rs:729, agents/orchestrator/delegation.rs:797, agents/orchestrator/recovery.rs
- `ChannelInboundMessage`: 14 处 — agents/agent.rs:1996, agents/ask_router.rs, agents/delegation_coordinator.rs:817, agents/orchestrator/ctx.rs:259, agents/orchestrator/ctx.rs:268, agents/orchestrator/delegation.rs, agents/orchestrator/event.rs:21, agents/orchestrator/event.rs:50
- `Channel`: 13 处 — agents/commands/mod.rs:62, agents/orchestrator/ctx.rs, agents/orchestrator/delegation.rs:868, agents/orchestrator/delegation.rs:913, agents/orchestrator/delegation.rs:951, agents/orchestrator/delegation.rs:991, agents/orchestrator/mod.rs, agents/orchestrator/test_support.rs
- `ChannelOutboundMessage`: 11 处 — agents/commands/friends.rs, agents/orchestrator/recovery.rs, agents/orchestrator/scheduled.rs:259, agents/orchestrator/test_support.rs, agents/scheduling/scheduler.rs, agents/session_context.rs:1087, agents/session_context.rs:1179, agents/session_context.rs:866
- `TurnStream`: 10 处 — agents/agent.rs:1484, agents/agent.rs:1510, agents/agent.rs:2285, agents/agent.rs:2312, agents/agent.rs:2333, agents/agent.rs:2360, agents/agent.rs:2381, agents/agent.rs:2399
- `MessageSender`: 9 处 — agents/agent.rs:1998, agents/ask_router.rs:130, agents/delegation_coordinator.rs:819, agents/orchestrator/delegation.rs:536, agents/orchestrator/delegation.rs:721, agents/orchestrator/delegation.rs:795, agents/orchestrator/inbound.rs:1044, agents/orchestrator/scheduled.rs:53
- `OutboundSendResult`: 3 处 — agents/orchestrator/test_support.rs, agents/skill_extract.rs:499, agents/skill_extract.rs:501
- `PersistedChannelMessage`: 3 处 — agents/session/backend.rs:377, agents/session/backend.rs:466, agents/session/types.rs
- `StreamDelivery`: 3 处 — agents/session_context.rs:1059, agents/session_context.rs:1067, agents/session_context.rs:1068
- `ToolEvent`: 2 处 — agents/agent.rs:771, agents/agent.rs:895
- `FoldCandidate`: 2 处 — agents/session_context.rs:542, agents/session_context.rs:723
- `(module)`: 1 处 — agents/orchestrator/inbound.rs
- `ChannelFile`: 1 处 — agents/session_context.rs:1158
- `ChannelFileMeta`: 1 处 — agents/session_context.rs:1159
- `LocalFileBody`: 1 处 — agents/session_context.rs:1173
- `InlineButton`: 1 处 — agents/tool_executor.rs

## agents → config

- `agent`: 28 处 — agents/agent.rs, agents/agent.rs:1909, agents/attachment.rs:1004, agents/attachment.rs:1012, agents/attachment.rs:456, agents/attachment.rs:460, agents/attachment.rs:461, agents/attachment.rs:462
- `filters`: 14 处 — agents/delegation_coordinator.rs:1834, agents/delegation_coordinator.rs:1835, agents/delegation_coordinator.rs:1836, agents/orchestrator/recovery.rs:829, agents/orchestrator/recovery.rs:830, agents/orchestrator/recovery.rs:831, agents/session/manager.rs:774, agents/workspace/agent_loader.rs:166
- `scheduler`: 10 处 — agents/orchestrator/mod.rs:65, agents/orchestrator/scheduled.rs:88, agents/scheduling/cron_loader.rs, agents/scheduling/cron_loader.rs:42, agents/scheduling/scheduler.rs, agents/scheduling/scheduler.rs:120, agents/scheduling/scheduler.rs:1368, agents/scheduling/scheduler.rs:184
- `sub_agent`: 7 处 — agents/agent.rs, agents/agent_registry.rs, agents/delegation_coordinator.rs, agents/media_e2e_test.rs, agents/orchestrator/recovery.rs, agents/session/manager.rs, agents/workspace/agent_loader.rs
- `mcp`: 4 处 — agents/mcp_manager.rs, agents/mcp_manager.rs:148, agents/mcp_manager.rs:149, agents/mcp_manager.rs:41
- `routing`: 2 处 — agents/media_e2e_test.rs, agents/media_e2e_test.rs:166
- `users_root`: 2 处 — agents/user_registry.rs:163, agents/user_registry.rs:166
- `known_users_path`: 1 处 — agents/known_users.rs:201
- `inbound_spool_dir`: 1 处 — agents/orchestrator/mod.rs:298
- `completion_queue_dir`: 1 处 — agents/orchestrator/mod.rs:351
- `memory_distill_state_dir`: 1 处 — agents/orchestrator/scheduled.rs:288
- `user_resolver_path`: 1 处 — agents/user_profile.rs:51

## agents → storage

- `DelegationCheckpoint`: 19 处 — agents/delegation_coordinator.rs:1521, agents/delegation_coordinator.rs:2036, agents/delegation_coordinator.rs:2037, agents/delegation_coordinator.rs:2276, agents/delegation_coordinator.rs:2353, agents/delegation_coordinator.rs:2383, agents/delegation_coordinator.rs:2456, agents/delegation_coordinator.rs:2511
- `CompletionNoticeStore`: 8 处 — agents/orchestrator/ctx.rs:173, agents/orchestrator/delegation.rs:866, agents/orchestrator/delegation.rs:911, agents/orchestrator/delegation.rs:949, agents/orchestrator/delegation.rs:989, agents/orchestrator/mod.rs:352, agents/orchestrator/recovery.rs:889, agents/orchestrator/recovery.rs:926
- `InboundSpool`: 6 处 — agents/orchestrator/ctx.rs:180, agents/orchestrator/inbound.rs, agents/orchestrator/mod.rs, agents/orchestrator/mod.rs:299, agents/orchestrator/recovery.rs:1270, agents/orchestrator/recovery.rs:1279
- `SessionInfo`: 5 处 — agents/orchestrator/mod.rs:504, agents/orchestrator/recovery.rs:173, agents/recovery.rs:50, agents/session/backend.rs, agents/session/manager.rs
- `SessionBackend`: 4 处 — agents/delegation_coordinator.rs:2015, agents/mod.rs:52, agents/session/backend.rs, agents/session/manager.rs
- `DeliveryState`: 4 处 — agents/orchestrator/delegation.rs:491, agents/orchestrator/delegation.rs:896, agents/orchestrator/recovery.rs:902, agents/orchestrator/recovery.rs:945
- `SummaryRecord`: 3 处 — agents/commands/session.rs:148, agents/context_engine.rs, agents/session/backend.rs
- `CompletionNoticeEntry`: 3 处 — agents/orchestrator/delegation.rs:480, agents/orchestrator/recovery.rs:892, agents/orchestrator/recovery.rs:935
- `JsonFileBackend`: 1 处 — agents/delegation_coordinator.rs:2016
- `SavedSessionFile`: 1 处 — agents/session/backend.rs
- `session_file_name`: 1 处 — agents/session/backend.rs:281
- `write_session_file`: 1 处 — agents/session/backend.rs:285

## daemon → tools

- `shell`: 5 处 — daemon.rs:1173, daemon.rs:506, daemon.rs:512, daemon.rs:513, daemon.rs:532
- `TaskBoards`: 2 处 — daemon.rs:509, daemon.rs:557
- `SendMessageTool`: 2 处 — daemon.rs:510, daemon.rs:549
- `FriendToolsCtx`: 2 处 — daemon.rs:511, daemon.rs:619
- `ToolSearchTool`: 2 处 — daemon.rs:1264, daemon.rs:1334
- `builtin_tools`: 1 处 — daemon.rs:516
- `AskUserTool`: 1 处 — daemon.rs:544
- `ListDirTool`: 1 处 — daemon.rs:553
- `new_task_tools`: 1 处 — daemon.rs:561
- `SkillTool`: 1 处 — daemon.rs:566
- `SkillsListTool`: 1 处 — daemon.rs:569
- `SkillManageTool`: 1 处 — daemon.rs:577
- `CronJobTool`: 1 处 — daemon.rs:584
- `MemoryListTool`: 1 处 — daemon.rs:592
- `MemoryViewTool`: 1 处 — daemon.rs:596
- `MemorySearchTool`: 1 处 — daemon.rs:600
- `MemoryManageTool`: 1 处 — daemon.rs:604
- `SessionQueryTool`: 1 处 — daemon.rs:611
- `FriendRequestTool`: 1 处 — daemon.rs:624
- `FriendAcceptTool`: 1 处 — daemon.rs:627
- `FriendDeclineTool`: 1 处 — daemon.rs:630
- `FriendListTool`: 1 处 — daemon.rs:633
- `shell_env`: 1 处 — daemon.rs:892
- `SearchProviderCooldown`: 1 处 — daemon.rs:1209
- `WebSearchTool`: 1 处 — daemon.rs:1210
- `ViewImageTool`: 1 处 — daemon.rs:1217
- `HearAudioTool`: 1 处 — daemon.rs:1221
- `ViewVideoTool`: 1 处 — daemon.rs:1225
- `AgentDelegateTool`: 1 处 — daemon.rs:1299
- `AgentListTool`: 1 处 — daemon.rs:1315
- `AgentKillTool`: 1 处 — daemon.rs:1318
- `AgentResumeTool`: 1 处 — daemon.rs:1324
- `SessionsYieldTool`: 1 处 — daemon.rs:1331

## agents → ids

- `dir_name`: 15 处 — agents/scheduling/scheduler.rs:1216, agents/scheduling/scheduler.rs:2928, agents/scheduling/scheduler.rs:3008, agents/scheduling/scheduler.rs:3039, agents/scheduling/scheduler.rs:3088, agents/scheduling/scheduler.rs:3202, agents/scheduling/scheduler.rs:3229, agents/scheduling/scheduler.rs:3270
- `bare_dir_name`: 10 处 — agents/agent.rs:1389, agents/agent.rs:1396, agents/agent.rs:1405, agents/agent.rs:2437, agents/agent.rs:2466, agents/agent.rs:2502, agents/scheduling/scheduler.rs:1194, agents/scheduling/scheduler.rs:2335
- `Fqid`: 6 处 — agents/commands/friends.rs:200, agents/commands/friends.rs:238, agents/delegation_coordinator.rs, agents/scheduling/scheduler.rs:1027, agents/session/backend.rs:62, agents/user_registry.rs
- `TYPE_MSG`: 2 处 — agents/commands/friends.rs:200, agents/commands/friends.rs:238
- `TYPE_SESSION`: 2 处 — agents/delegation_coordinator.rs, agents/session/backend.rs:62
- `TYPE_JOB`: 1 处 — agents/scheduling/scheduler.rs:1027
- `DEFAULT_NAMESPACE`: 1 处 — agents/session/backend.rs:32
- `TYPE_USER`: 1 处 — agents/user_registry.rs

## agents → tools

- `shell`: 26 处 — agents/agent.rs:1072, agents/agent.rs:1081, agents/delegation_coordinator.rs:219, agents/delegation_coordinator.rs:222, agents/delegation_coordinator.rs:232, agents/delegation_coordinator.rs:456, agents/delegation_coordinator.rs:860, agents/orchestrator/ctx.rs:183
- `TaskBoards`: 4 处 — agents/context_engine.rs:132, agents/context_engine.rs:351, agents/runtime.rs:155, agents/runtime.rs:92
- `builtin_tools`: 1 处 — agents/media_e2e_test.rs:103
- `ViewImageTool`: 1 处 — agents/media_e2e_test.rs:107
- `HearAudioTool`: 1 处 — agents/media_e2e_test.rs:111
- `ViewVideoTool`: 1 处 — agents/media_e2e_test.rs:115
- `SearchProviderCooldown`: 1 处 — agents/runtime.rs
- `truncation`: 1 处 — agents/tool_executor.rs:342

## daemon → agents

- `scheduling`: 3 处 — daemon.rs:1093, daemon.rs:1110, daemon.rs:1118
- `recovery`: 3 处 — daemon.rs:1367, daemon.rs:2023, daemon.rs:2024
- `UserResolver`: 2 处 — daemon.rs:1134, daemon.rs:499
- `AskRouter`: 2 处 — daemon.rs:1139, daemon.rs:500
- `KnownUsersRegistry`: 2 处 — daemon.rs:1145, daemon.rs:501
- `UserRegistry`: 2 处 — daemon.rs:1155, daemon.rs:502
- `loop_breaker`: 2 处 — daemon.rs:1531, daemon.rs:1532
- `AgentDelegator`: 1 处 — daemon.rs
- `(module)`: 1 处 — daemon.rs
- `SharedScheduler`: 1 处 — daemon.rs:496
- `skill_loader`: 1 处 — daemon.rs:660
- `agent_loader`: 1 处 — daemon.rs:680
- `SchedulerEvent`: 1 处 — daemon.rs:1079
- `AgentRegistry`: 1 处 — daemon.rs:1202
- `WorkspaceWatcher`: 1 处 — daemon.rs:1239
- `DelegationEvent`: 1 处 — daemon.rs:1347
- `resource_provider`: 1 处 — daemon.rs:1510
- `context_engine`: 1 处 — daemon.rs:1519
- `tool_executor`: 1 处 — daemon.rs:1526
- `runtime`: 1 处 — daemon.rs:1538
- `AgentRuntime`: 1 处 — daemon.rs:1544
- `WebhookContext`: 1 处 — daemon.rs:1631
- `run_webhook_server`: 1 处 — daemon.rs:1674
- `session`: 1 处 — daemon.rs:1938

## channels → providers

- `media`: 21 处 — channels/client.rs:2247, channels/client.rs:2248, channels/client.rs:2287, channels/client.rs:2288, channels/client.rs:715, channels/client.rs:720, channels/client.rs:721, channels/client.rs:722
- `capability_tool`: 4 处 — channels/client.rs, channels/client.rs:1321, channels/client.rs:205, channels/client.rs:257
- `ProviderRegistry`: 4 处 — channels/client.rs, channels/client.rs:1326, channels/client.rs:218, channels/client.rs:282
- `capability_chat`: 2 处 — channels/client.rs, channels/client.rs:2234

## registry → providers

- `capability`: 4 处 — registry/mod.rs, registry/mod.rs:347, registry/mod.rs:57, registry/mod.rs:624
- `MediaPolicy`: 4 处 — registry/mod.rs:358, registry/mod.rs:366, registry/mod.rs:643, registry/mod.rs:74
- `Capability`: 3 处 — registry/mod.rs, registry/mod.rs:424, registry/routing.rs
- `ProviderSummary`: 2 处 — registry/mod.rs:654, registry/mod.rs:667
- `SharedApiKey`: 1 处 — registry/mod.rs
- `SharedCredentialPool`: 1 处 — registry/mod.rs
- `capability_chat`: 1 处 — registry/mod.rs
- `capability_embedding`: 1 处 — registry/mod.rs
- `image`: 1 处 — registry/mod.rs
- `provider_registry`: 1 处 — registry/mod.rs
- `search`: 1 处 — registry/mod.rs
- `stt`: 1 处 — registry/mod.rs
- `tts`: 1 处 — registry/mod.rs
- `video`: 1 处 — registry/mod.rs
- `FallbackChatProvider`: 1 处 — registry/mod.rs
- `fallback`: 1 处 — registry/mod.rs
- `ProviderId`: 1 处 — registry/mod.rs:348
- `MediaLoweringProvider`: 1 处 — registry/mod.rs:373

## agents → memory

- `IndexEntry`: 11 处 — agents/agent.rs:306, agents/agent.rs:310, agents/attachment.rs:264, agents/attachment.rs:266, agents/attachment.rs:317, agents/memory_distill.rs:309, agents/memory_distill.rs:310, agents/memory_fork.rs:498
- `scan_memory_files`: 8 处 — agents/agent.rs:309, agents/memory_distill.rs:183, agents/memory_distill.rs:215, agents/memory_distill.rs:272, agents/memory_distill.rs:305, agents/memory_distill.rs:318, agents/memory_fork.rs:494, agents/session_context.rs:823
- `format_full_memory_index`: 2 处 — agents/memory_distill.rs:311, agents/memory_fork.rs:500
- `should_inject`: 1 处 — agents/attachment.rs:268
- `MemoryFile`: 1 处 — agents/memory_distill.rs:213

## daemon → config

- `AppConfig`: 11 处 — daemon.rs:105, daemon.rs:134, daemon.rs:186, daemon.rs:43, daemon.rs:497, daemon.rs:59, daemon.rs:656, daemon.rs:677
- `sub_agent`: 4 处 — daemon.rs, daemon.rs:678, daemon.rs:691, daemon.rs:724
- `ConfigLoader`: 2 处 — daemon.rs:49, daemon.rs:66
- `agent`: 2 处 — daemon.rs:861, daemon.rs:862
- `filters`: 1 处 — daemon.rs
- `init_safety_config`: 1 处 — daemon.rs:882
- `telegram_offset_path`: 1 处 — daemon.rs:2005

## channels → config

- `channel`: 16 处 — channels/client.rs, channels/qqbot/channel.rs, channels/telegram/channel.rs, channels/telegram/channel.rs:114, channels/telegram/channel.rs:2249, channels/telegram/channel.rs:2303, channels/telegram/channel.rs:2467, channels/telegram/channel.rs:2515
- `agent`: 3 处 — channels/message.rs:322, channels/message.rs:335, channels/message.rs:381
- `default_base_dir`: 1 处 — channels/telegram/channel.rs:156
- `telegram_offset_path`: 1 处 — channels/telegram/channel.rs:201

## channels → agents

- `UserResolver`: 5 处 — channels/client.rs, channels/client.rs:1037, channels/client.rs:1327, channels/client.rs:221, channels/client.rs:288
- `SessionManager`: 4 处 — channels/client.rs, channels/client.rs:1320, channels/client.rs:203, channels/client.rs:252
- `SkillManager`: 4 处 — channels/client.rs, channels/client.rs:1325, channels/client.rs:216, channels/client.rs:277
- `TurnEvent`: 3 处 — channels/client.rs, channels/telegram/channel.rs, channels/turn_stream.rs
- `Skill`: 1 处 — channels/client.rs
- `workspace`: 1 处 — channels/client.rs
- `commands`: 1 处 — channels/client.rs:2167

## providers → agents

- `llm_stream`: 16 处 — providers/protocols/anthropic/messages.rs:100, providers/protocols/anthropic/messages.rs:106, providers/protocols/anthropic/messages.rs:116, providers/protocols/anthropic/messages.rs:86, providers/protocols/google/generate_content.rs:100, providers/protocols/google/generate_content.rs:110, providers/protocols/google/generate_content.rs:80, providers/protocols/google/generate_content.rs:94
- `session`: 1 处 — providers/capability_tool.rs

## tools → config

- `is_path_protected`: 11 处 — tools/file_ops.rs:125, tools/file_ops.rs:22, tools/file_ops.rs:530, tools/file_ops.rs:630, tools/hear_audio.rs:228, tools/list_dir.rs:179, tools/list_dir.rs:66, tools/search.rs:196
- `scheduler`: 4 处 — tools/cronjob_tool.rs:1308, tools/cronjob_tool.rs:1478, tools/cronjob_tool.rs:1576, tools/cronjob_tool.rs:334
- `memory_audit_dir`: 1 处 — tools/memory_tool.rs:93
- `ShellConfig`: 1 处 — tools/shell_env.rs

## channels → memory

- `MEMORY_DIR_NAME`: 5 处 — channels/client.rs:1344, channels/client.rs:1606, channels/client.rs:1686, channels/client.rs:1756, channels/client.rs:1837
- `MemoryFile`: 5 处 — channels/client.rs:1351, channels/client.rs:1612, channels/client.rs:1634, channels/client.rs:1769, channels/client.rs:1850
- `scan_memory_files`: 4 处 — channels/client.rs:1615, channels/client.rs:1625, channels/client.rs:1762, channels/client.rs:1843
- `build_backlinks`: 1 处 — channels/client.rs:1636

## registry → config

- `routing`: 9 处 — registry/mod.rs:180, registry/mod.rs:308, registry/mod.rs:309, registry/mod.rs:312, registry/mod.rs:313, registry/mod.rs:314, registry/mod.rs:315, registry/mod.rs:420
- `provider`: 5 处 — registry/mod.rs:179, registry/mod.rs:292, registry/mod.rs:293, registry/mod.rs:349, registry/mod.rs:360
- `ModelConfig`: 1 处 — registry/mod.rs:34

## tools → memory

- `MemoryFile`: 7 处 — tools/memory_tool.rs, tools/memory_tool.rs:156, tools/memory_tool.rs:157, tools/memory_tool.rs:251, tools/memory_tool.rs:272, tools/memory_tool.rs:347, tools/memory_tool.rs:892
- `scan_memory_files`: 3 处 — tools/memory_tool.rs:1147, tools/memory_tool.rs:157, tools/memory_tool.rs:348
- `IndexEntry`: 2 处 — tools/memory_tool.rs:649, tools/memory_tool.rs:656
- `LinkRef`: 1 处 — tools/memory_tool.rs
- `build_backlinks`: 1 处 — tools/memory_tool.rs
- `extract_links_from_content`: 1 处 — tools/memory_tool.rs:477

## agents → str_utils

- `extract_yaml_string`: 5 处 — agents/scheduling/cron_loader.rs, agents/scheduling/scheduler.rs:1319, agents/scheduling/scheduler.rs:1324, agents/scheduling/scheduler.rs:1332, agents/workspace/agent_loader.rs
- `parse_front_matter`: 3 处 — agents/scheduling/cron_loader.rs, agents/scheduling/scheduler.rs:1318, agents/workspace/agent_loader.rs
- `(module)`: 2 处 — agents/prompt.rs, agents/workspace/skill_loader.rs
- `neutralize_spoofing`: 1 处 — agents/session_context.rs:593
- `extract_yaml_list`: 1 处 — agents/workspace/agent_loader.rs
- `truncate_chars`: 1 处 — agents/workspace/skills.rs:73

## tools → str_utils

- `UNKNOWN_ID_LISTING_CAP`: 4 处 — tools/agent_kill.rs, tools/agent_resume.rs, tools/cronjob_tool.rs:923, tools/session_query.rs
- `neutralize_spoofing`: 3 处 — tools/shell.rs:371, tools/shell.rs:397, tools/shell.rs:951
- `truncate_line`: 2 处 — tools/agent_kill.rs:26, tools/task.rs:233
- `extract_yaml_string`: 2 处 — tools/skill_manage_tool.rs:453, tools/skill_manage_tool.rs:462
- `UNKNOWN_ID_PREVIEW_CHARS`: 1 处 — tools/agent_kill.rs
- `(module)`: 1 处 — tools/file_ops.rs

## tools → ids

- `DEFAULT_NAMESPACE`: 3 处 — tools/friends.rs, tools/send_message.rs, tools/task.rs
- `Fqid`: 3 处 — tools/friends.rs, tools/send_message.rs, tools/task.rs
- `TYPE_MSG`: 2 处 — tools/friends.rs, tools/send_message.rs
- `bare_dir_name`: 2 处 — tools/shell.rs:224, tools/task.rs
- `TYPE_TASK`: 1 处 — tools/task.rs

## agents → mcp

- `config_types`: 5 处 — agents/mcp_manager.rs, agents/mcp_manager.rs:148, agents/mcp_manager.rs:159, agents/mcp_manager.rs:47, agents/mcp_manager.rs:49
- `McpRegistry`: 4 处 — agents/mcp_manager.rs:18, agents/mcp_manager.rs:52, agents/mcp_manager.rs:89, agents/mcp_manager.rs:9
- `McpToolWrapper`: 1 处 — agents/mcp_manager.rs:96

## daemon → providers

- `Tool`: 2 处 — daemon.rs, daemon.rs:552
- `(module)`: 1 处 — daemon.rs
- `ProviderId`: 1 处 — daemon.rs
- `detect_from_url`: 1 处 — daemon.rs
- `well_known`: 1 处 — daemon.rs
- `ToolResult`: 1 处 — daemon.rs
- `Capability`: 1 处 — daemon.rs:157
- `media`: 1 处 — daemon.rs:887
- `ProviderRegistry`: 1 处 — daemon.rs:1206

## providers → config

- `provider`: 10 处 — providers/media.rs:367, providers/media.rs:377, providers/media.rs:383, providers/media.rs:389, providers/media.rs:395, providers/media.rs:406, providers/media.rs:419, providers/media.rs:708

## storage → channels

- `PersistedChannelMessage`: 6 处 — storage/inbound_spool.rs, storage/json_file.rs:101, storage/json_file.rs:1105, storage/json_file.rs:1117, storage/session.rs:314, storage/session.rs:323
- `ChannelInboundMessage`: 1 处 — storage/inbound_spool.rs
- `ChannelMessageContent`: 1 处 — storage/inbound_spool.rs
- `MessageReceiver`: 1 处 — storage/inbound_spool.rs
- `MessageSender`: 1 处 — storage/inbound_spool.rs

## config → providers

- `Capability`: 3 处 — config/mod.rs, config/mod.rs:1003, config/routing.rs
- `AuthStyle`: 3 处 — config/provider.rs:50, config/provider.rs:53, config/provider.rs:54
- `RotationStrategy`: 1 处 — config/provider.rs:11
- `capability`: 1 处 — config/provider.rs:12

## daemon → hot_switch

- `is_hot_switch`: 3 处 — daemon.rs:1737, daemon.rs:535, daemon.rs:924
- `inherited_socket_fd`: 1 处 — daemon.rs:931
- `inherited_client_socket_fd`: 1 处 — daemon.rs:938
- `mark_new_process_ready`: 1 处 — daemon.rs:1728
- `old_pid`: 1 处 — daemon.rs:1760
- `do_hot_switch`: 1 处 — daemon.rs:1825

## migration → ids

- `dir_name`: 1 处 — migration.rs
- `Fqid`: 1 处 — migration.rs
- `TYPE_JOB`: 1 处 — migration.rs
- `TYPE_SESSION`: 1 处 — migration.rs
- `TYPE_TASK`: 1 处 — migration.rs
- `TYPE_USER`: 1 处 — migration.rs
- `id_from_dir`: 1 处 — migration.rs
- `is_known_type`: 1 处 — migration.rs:877

## mcp → agents

- `session`: 7 处 — mcp/deferred.rs:309, mcp/deferred.rs:347, mcp/deferred.rs:382, mcp/tool.rs:184, mcp/tool.rs:225, mcp/tool.rs:250, mcp/tool.rs:71

## storage → ids

- `bare_dir_name`: 3 处 — storage/json_file.rs, storage/session.rs:157, storage/session.rs:183
- `id_from_dir`: 1 处 — storage/json_file.rs
- `Fqid`: 1 处 — storage/json_file.rs
- `DEFAULT_NAMESPACE`: 1 处 — storage/json_file.rs
- `TYPE_SESSION`: 1 处 — storage/json_file.rs

## tools → channels

- `ChannelMessageContent`: 2 处 — tools/ask_user.rs, tools/friends.rs
- `ChannelOutboundMessage`: 2 处 — tools/ask_user.rs, tools/friends.rs
- `MessageReceiver`: 2 处 — tools/ask_user.rs, tools/friends.rs
- `(module)`: 1 处 — tools/send_message.rs

## daemon → channels

- `WebSocketChannel`: 2 处 — daemon.rs:1435, daemon.rs:1437
- `Channel`: 1 处 — daemon.rs
- `telegram`: 1 处 — daemon.rs:811
- `wechat`: 1 处 — daemon.rs:828
- `qqbot`: 1 处 — daemon.rs:846

## agents → hot_switch

- `is_hot_switch`: 1 处 — agents/orchestrator/mod.rs:527
- `old_pid`: 1 处 — agents/orchestrator/mod.rs:528
- `wait_for_old_process_exit`: 1 处 — agents/orchestrator/mod.rs:529
- `RECOVERY_WAIT_OLD_TIMEOUT`: 1 处 — agents/orchestrator/mod.rs:531

## daemon → storage

- `SessionBackend`: 2 处 — daemon.rs:504, daemon.rs:775
- `JsonFileBackend`: 1 处 — daemon.rs:777
- `DelegationCheckpoint`: 1 处 — daemon.rs:1373

## mcp → providers

- `ToolSource`: 2 处 — mcp/tool.rs:57, mcp/tool.rs:65
- `tool`: 1 处 — mcp/tool_trait.rs:3
- `capability_tool`: 1 处 — mcp/tool_trait.rs:7

## storage → providers

- `ChatMessage`: 2 处 — storage/json_file.rs, storage/session.rs:8
- `ContentPart`: 1 处 — storage/json_file.rs
- `capability_chat`: 1 处 — storage/session.rs:80

## agents → registry

- `routing`: 2 处 — agents/media_e2e_test.rs, agents/media_e2e_test.rs:153
- `Registry`: 1 处 — agents/media_e2e_test.rs

## daemon → signal

- `pid_file_path`: 3 处 — daemon.rs:1846, daemon.rs:1876, daemon.rs:964

## tools → storage

- `DelegationCheckpoint`: 1 处 — tools/agent_resume.rs
- `SessionBackend`: 1 处 — tools/session_query.rs
- `SessionInfo`: 1 处 — tools/session_query.rs

## agents → sys_info

- `runtime_info`: 2 处 — agents/prompt.rs:181, agents/prompt.rs:186

## daemon → registry

- `Registry`: 2 处 — daemon.rs:186, daemon.rs:197

## migration → config

- `default_base_dir`: 1 处 — migration.rs:214
- `user_resolver_path`: 1 处 — migration.rs:375

## config → agents

- `loop_breaker`: 1 处 — config/mod.rs

## daemon → memory

- `ensure_memory_dir`: 1 处 — daemon.rs:972

## daemon → migration

- `run_auto`: 1 处 — daemon.rs:1040

## daemon → update_state

- `UpdateState`: 1 处 — daemon.rs:1770

## tools → hot_switch

- `pid_alive`: 1 处 — tools/shell.rs:272
