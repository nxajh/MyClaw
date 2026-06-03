# MyClaw 多模态辅助翻译方案

> RFC v1 · 2026-06-03

## 1. 背景与问题

### 1.1 现状

MyClaw 的多模态基础设施已部分就绪：

- `Modality` 枚举已声明 `Text / Image / Audio / Video` 四种模态
- `ChatModelConfig.input: Vec<Modality>` 可声明模型支持哪些输入模态
- `Capability` 枚举覆盖 `Chat / ImageGeneration / TextToSpeech / SpeechToText / VideoGeneration / Search`
- `RoutingConfig` 为每种 Capability 提供独立路由（`[routing.speech_to_text]` 等）
- `agent.rs` 已实现 `model_supports_images` 检查，视觉模型可原生接收图片
- `ContentPart` 枚举支持 `ImageUrl / ImageB64 / Thinking` 等多种内容类型
- `context_engine.rs` 的 compaction 流程有 `strip_images()` 将图片替换为 `[image]`

### 1.2 缺失

当主模型**不支持**某模态时（如 DeepSeek V3 不支持图片），系统无法处理：

- **图片**：当前代码在 `model_supports_images == false` 时静默跳过图片附加，主模型完全不知道用户发了图
- **音频**：`Modality::Audio` 已声明但无任何处理逻辑
- **视频**：同上

用户在 Telegram 发送图片给 DeepSeek V3 时，主模型看不到图片内容，无法回答相关问题。

## 2. 行业调研

### 2.1 主流模型能力矩阵（2025-2026）

| 模型 | 图片理解(input) | 图片生成(output) | 音频理解 | 音频生成 | 视频理解 |
|------|:-:|:-:|:-:|:-:|:-:|
| GPT-4o / GPT-5.x | ✅ | ✅ native | ✅ | ✅ | ❌ |
| gpt-image-1 | ✅ | ✅ | ❌ | ❌ | ❌ |
| Claude Sonnet 4 | ✅ | ❌ | ❌ | ❌ | ❌ |
| Gemini 2.5 Pro | ✅ | ✅ native | ✅ | ✅ TTS | ✅ |
| Gemini 2.5 Flash | ✅ | ✅ | ✅ | ✅ | ✅ |
| Qwen3-VL | ✅ | ❌ | ✅ | ❌ | ✅ |
| DeepSeek V3 | ❌ | ❌ | ❌ | ❌ | ❌ |
| DeepSeek VL2 | ✅ | ❌ | ❌ | ❌ | ❌ |

**关键发现**：图片理解 ≠ 图片生成，且两者越来越多由同一模型承担（GPT-4o、Gemini 2.5），但也经常分离（Claude 只理解不生成）。图片理解本质上是 Chat 模型的 input modality，不是独立能力。

### 2.2 各项目方案对比

#### Hermes（最完整）

- `agent/image_routing.py`：`decide_image_input_mode()` 返回 `"native"` 或 `"text"`
- **native 模式**：图片作为 `image_url` content part 直接附在消息上
- **text 模式**（非视觉模型）：
  1. 对每张图片调用 `vision_analyze_tool`（使用辅助模型）
  2. 将图片 content part 替换为文本描述
  3. 对**整个消息历史**做 `copy.deepcopy` 后替换，不修改持久化数据
- 辅助模型通过 `agent/auxiliary_client.py` 全局路由（OpenRouter → Anthropic → 自定义）
- compaction 时替换为 `"[Attached image — stripped after compression]"`
- 非视觉模型 fallback 时生成 `"[The user attached an image. Here's what it contains:\n{description}]"`

#### OpenClaw（独立子系统）

- `src/media-understanding/`：独立模块，处理图片/音频/视频
- **在 agent 回复前预处理**：所有媒体附件先翻译为文本，注入 `ctx.Body`
- 主模型只看到文本，完全不知道原始是图片/音频/视频
- 同时支持 native vision：视觉模型时图片直接注入 prompt
- context pruning 时替换为 `"[image removed during context pruning]"`
- 有 `provider-registry` 管理 media understanding provider

#### Codex（最简单）

- 只用 OpenAI 系模型（天然支持视觉）
- `InputModality` 只声明 `"text" | "image"`
- 不做辅助翻译，无 compaction

### 2.3 共同设计模式

| 模式 | 所有项目的共识 |
|------|--------------|
| 持久化历史不变 | 只在构建 API 请求时对 clone 的消息做变换 |
| 不保留 URL | 替换为纯文本占位符，LLM 无法访问 URL |
| 当轮图片需要翻译 | 非视觉模型必须通过辅助模型理解图片内容 |
| 历史旧图片直接替换 | 上下文已过期，翻译无意义，直接用占位符 |
| compaction 统一替换 | 压缩时所有图片统一变为文本占位符 |

## 3. 设计决策

### 3.1 不新增 Capability

图片理解/音频理解是 Chat 模型的 **input modality**，不是独立能力。

```
图片理解 = ChatModelConfig.input 包含 Modality::Image
图片生成 = 独立的 ImageGeneration capability（不同 API endpoint）
```

现有 `Capability` 枚举不需要新增 `ImageUnderstanding` / `AudioUnderstanding`。

**理由**：
- 图片理解调用的是 Chat Completions API，不是独立 endpoint
- 如果为每种 input modality 新增 Capability，会膨胀为 `Chat / ChatWithImage / ChatWithAudio / ChatWithVideo`，没有意义
- 现有 `ChatModelConfig.input: Vec<Modality>` 已经是正确的能力声明方式

### 3.2 不新增配置

现有配置已完全支持多模型多模态声明：

```toml
# 主模型（纯文本）
[providers.deepseek.chat.models.deepseek-v3]
input = ["text"]
output = ["text"]

# 辅助视觉模型
[providers.openai.chat.models.gpt-4o-mini]
input = ["text", "image"]
output = ["text"]

# 辅助音频模型
[providers.openai.chat.models.gpt-4o-audio]
input = ["text", "audio"]
output = ["text"]

# 路由
[routing.chat]
strategy = "fallback"
models = ["deepseek-v3", "gpt-4o-mini"]
```

运行时从 registry 中查找支持特定 modality 的已注册 Chat 模型即可，零配置新增。

### 3.3 辅助翻译 = 临时 Chat 调用

不需要独立的 `ImageUnderstandingProvider` trait。辅助翻译就是一次普通的 Chat API 调用：

```
辅助调用 = 单条 user 消息（图片 + "请描述这张图片"）→ Chat API → 文本描述
```

**不发历史消息**，因为：
- 避免不同协议间的 Thinking blocks / tool_calls 兼容性问题
- 图片描述是自包含的，不需要对话上下文
- 减少延迟和 token 消耗

### 3.4 消息变换发生在 clone 层

```
Layer 1: 持久化历史 (history.jsonl) — 永不修改
         ↓ session.history.iter().cloned()
Layer 2: messages: Vec<ChatMessage> — clone，可修改
         ↓ sanitize_history()          [已有]
         ↓ modality_adapt()            [新增]
         ↓ 图片附加（如果模型支持）     [已有]
Layer 3: 协议渲染 render_*_body()      [已有]
```

和 Hermes 的 `copy.deepcopy(api_messages)` 是同一思路。

## 4. 详细设计

### 4.1 新增方法：`supports_modality()`

```rust
// src/providers/capability.rs

impl ChatModelConfig {
    pub fn supports_image_input(&self) -> bool {
        self.input.contains(&Modality::Image)
    }

    /// Whether the model supports audio input.
    pub fn supports_audio_input(&self) -> bool {
        self.input.contains(&Modality::Audio)
    }

    /// Whether the model supports video input.
    pub fn supports_video_input(&self) -> bool {
        self.input.contains(&Modality::Video)
    }

    /// Whether the model supports the given input modality.
    pub fn supports_input(&self, modality: Modality) -> bool {
        self.input.contains(&modality)
    }
}
```

### 4.2 新增方法：`find_chat_model_with_modality()`

```rust
// src/registry/mod.rs — impl ProviderRegistry for Registry

/// Find a registered chat model that supports the given input modality.
/// Searches the routing fallback chain first, then all registered models.
fn find_chat_model_with_modality(
    &self,
    modality: Modality,
) -> Option<(Arc<dyn ChatProvider>, String)> {
    // Priority 1: models in the chat routing chain
    for model_id in self.get_chat_routing_models() {
        if let Ok(cfg) = self.get_chat_model_config(&model_id) {
            if cfg.input.contains(&modality) {
                if let Some(provider) = self.chat_providers.get(&model_id) {
                    return Some((Arc::clone(provider), model_id.clone()));
                }
            }
        }
    }
    // Priority 2: any registered chat model
    for (model_id, provider) in &self.chat_providers {
        if let Ok(cfg) = self.get_chat_model_config(model_id) {
            if cfg.input.contains(&modality) {
                return Some((Arc::clone(provider), model_id.clone()));
            }
        }
    }
    None
}
```

### 4.3 新增模块：`modality_adapter`

```rust
// src/agents/modality_adapter.rs

//! Modality adaptation layer — translates non-text modalities to text
//! when the primary chat model does not support them natively.
//!
//! Operates on cloned messages only (never mutates persistent history).

use crate::providers::capability_chat::{ChatMessage, ContentPart};
use crate::providers::capability::Modality;
use crate::providers::capability_chat::ChatProvider;
use std::sync::Arc;

/// Result of modality adaptation for one message.
pub struct AdaptationResult {
    /// The adapted messages (may have image/audio parts replaced).
    pub messages: Vec<ChatMessage>,
    /// Whether any translation was performed.
    pub translated: bool,
}

/// Translate a single image to text using an auxiliary vision model.
async fn translate_image(
    provider: &dyn ChatProvider,
    model_id: &str,
    image_part: &ContentPart,
    prompt: &str,
) -> anyhow::Result<String> {
    let image_content_part = match image_part {
        ContentPart::ImageUrl { url, detail } => {
            ContentPart::ImageUrl { url: url.clone(), detail: *detail }
        }
        ContentPart::ImageB64 { b64_json, media_type, detail } => {
            ContentPart::ImageB64 {
                b64_json: b64_json.clone(),
                media_type: media_type.clone(),
                detail: *detail,
            }
        }
        _ => unreachable!(),
    };

    let user_msg = ChatMessage {
        role: "user".into(),
        parts: vec![
            image_content_part,
            ContentPart::Text { text: prompt.into() },
        ],
        name: None,
        tool_call_id: None,
        tool_calls: None,
        is_error: None,
    };

    let req = ChatRequest {
        model: model_id,
        messages: &[user_msg],
        temperature: Some(0.3),
        max_tokens: Some(1024),
        thinking: None,
        stop: None,
        seed: None,
        tools: None,
        stream: false,
    };

    let response = provider.chat(req)?;
    // Extract text from response
    Ok(response.text)
}

/// Adapt messages for a model that lacks the specified modality.
///
/// For **current-turn** media (the last user message):
///   - Translates via auxiliary model and prepends description
///
/// For **historical** media (all other messages):
///   - Replaces with `"[image]"` placeholder (no translation needed)
pub async fn adapt_messages(
    messages: &mut Vec<ChatMessage>,
    primary_model_id: &str,
    primary_config: &ChatModelConfig,
    aux_provider: Option<(Arc<dyn ChatProvider>, String)>,
) {
    let needs_image_adapt = !primary_config.supports_image_input()
        && messages.iter().any(|m| m.parts.iter().any(|p|
            matches!(p, ContentPart::ImageUrl { .. } | ContentPart::ImageB64 { .. })
        ));

    if !needs_image_adapt {
        return;
    }

    // Find the last user message index — that's the "current turn"
    let last_user_idx = messages.iter().rposition(|m| m.role == "user");

    for (i, msg) in messages.iter_mut().enumerate() {
        let has_images = msg.parts.iter().any(|p|
            matches!(p, ContentPart::ImageUrl { .. } | ContentPart::ImageB64 { .. })
        );
        if !has_images { continue; }

        let is_current_turn = last_user_idx == Some(i);

        if is_current_turn {
            // Current turn: translate via auxiliary model
            if let Some((ref provider, ref model_id)) = aux_provider {
                let mut descriptions = Vec::new();
                for part in &msg.parts {
                    match part {
                        ContentPart::ImageUrl { .. } | ContentPart::ImageB64 { .. } => {
                            match translate_image(
                                provider.as_ref(),
                                model_id,
                                part,
                                "Describe this image in detail, including text, objects, layout, and any notable visual information.",
                            ).await {
                                Ok(desc) => descriptions.push(desc),
                                Err(e) => {
                                    tracing::warn!(err = %e, "auxiliary image translation failed");
                                    descriptions.push("[image translation failed]".into());
                                }
                            }
                        }
                        _ => {}
                    }
                }
                if !descriptions.is_empty() {
                    // Prepend description, then replace image parts
                    let desc_text = if descriptions.len() == 1 {
                        format!("[图片描述]: {}", descriptions[0])
                    } else {
                        descriptions.iter().enumerate()
                            .map(|(i, d)| format!("[图片{}描述]: {}", i + 1, d))
                            .collect::<Vec<_>>()
                            .join("\n")
                    };
                    // Replace image parts with text
                    msg.parts = msg.parts.drain(..).map(|part| match part {
                        ContentPart::ImageUrl { .. } | ContentPart::ImageB64 { .. } => {
                            ContentPart::Text { text: String::new() }
                        }
                        other => other,
                    }).collect();
                    // Insert description at the beginning
                    msg.parts.insert(0, ContentPart::Text { text: desc_text });
                    // Clean up empty text parts
                    msg.parts.retain(|p| !matches!(p, ContentPart::Text { text } if text.is_empty()));
                }
            } else {
                // No auxiliary model available: simple placeholder
                for part in &mut msg.parts {
                    if matches!(part, ContentPart::ImageUrl { .. } | ContentPart::ImageB64 { .. }) {
                        *part = ContentPart::Text { text: "[image — no vision model available]".into() };
                    }
                }
            }
        } else {
            // Historical message: simple placeholder
            for part in &mut msg.parts {
                match part {
                    ContentPart::ImageUrl { .. } | ContentPart::ImageB64 { .. } => {
                        *part = ContentPart::Text { text: "[image]".into() };
                    }
                    _ => {}
                }
            }
        }
    }
}
```

### 4.4 修改 `agent.rs`：集成模态适配

```rust
// src/agents/agent.rs — run() 方法中

// 现有代码：构建 messages
let mut messages: Vec<ChatMessage> = std::iter::once(system_msg.clone())
    .chain(session.history.iter().cloned())
    .collect();
sanitize_history(&mut messages);

// ===== 新增：模态适配 =====
let model_config = runtime.providers
    .get_chat_model_config(model_id)
    .ok();

let aux_vision = if model_config.as_ref().map(|c| c.supports_image_input()).unwrap_or(false) {
    None // 主模型支持图片，不需要辅助
} else {
    // 从 registry 找一个支持图片的 chat 模型
    runtime.providers.find_chat_model_with_modality(Modality::Image)
};

if let Some(ref config) = model_config {
    modality_adapter::adapt_messages(
        &mut messages,
        model_id,
        config,
        aux_vision,
    ).await;
}
// ===== 结束 =====

// 现有代码：图片附加（仅在主模型支持时）
if !images_attached && model_supports_images {
    // ... 现有逻辑不变
}
```

### 4.5 ProviderRegistry trait 扩展

```rust
// src/providers/provider_registry.rs — 新增方法

/// Find a registered chat model that supports the given input modality.
fn find_chat_model_with_modality(
    &self,
    modality: Modality,
) -> Option<(Arc<dyn ChatProvider>, String)>;
```

## 5. 数据流图

### 5.1 主模型支持图片（现有流程，不变）

```
用户发图 + 文本
    ↓
session.add_user(text)
session.last_message.image_urls = [url]
    ↓
messages = clone(history)
    ↓
model_supports_images = true
    ↓
附加 ImageUrl parts 到最后一条 user 消息
    ↓
render → API（带图片）
    ↓
持久化：history 中完整保存 ImageUrl parts
```

### 5.2 主模型不支持图片（新增流程）

```
用户发图 + 文本
    ↓
session.add_user(text)
session.last_message.image_urls = [url]
    ↓
messages = clone(history)
    ↓
model_supports_images = false
    ↓
adapt_messages():
  ├─ 历史 messages 中的 ImageUrl → "[image]"
  └─ 当轮新图片:
       ├─ 找到辅助视觉模型 (gpt-4o-mini)
       ├─ 单次 Chat 调用: "描述这张图片"
       ├─ 拿到文本描述
       └─ 注入: "[图片描述]: 一张包含...的图片"
    ↓
render → API（纯文本）
    ↓
持久化：history 中仍然完整保存 ImageUrl parts（不变）
```

### 5.3 切换模型场景

```
Turn 1: 用户用 GPT-4o（支持视觉）
  history[0] = user:  {Text, ImageUrl: "https://a.jpg"}
  history[1] = assistant: {Text: "这是一张..."}

Turn 2: 用户切换到 DeepSeek V3（不支持视觉）
  messages = clone(history)
  adapt_messages():
    history[0] ImageUrl → "[image]"     ← 历史，简单替换
    当轮新图片 → 辅助模型翻译             ← 当轮，翻译
  持久化 history 不变，ImageUrl 仍在
```

## 6. 处理策略总结

| 内容类型 | 场景 | 处理方式 |
|---------|------|---------|
| 当轮新图片 | 主模型支持 | 原生附加 ImageUrl parts |
| 当轮新图片 | 主模型不支持 | 辅助模型翻译 → 文本描述注入 |
| 当轮新图片 | 无辅助模型 | `"[image — no vision model available]"` |
| 历史中的图片 | 发给非视觉模型 | `"[image]"` |
| 历史中的图片 | 发给视觉模型 | 保持原样 |
| Compaction | 所有图片 | `"[image]"`（已有逻辑）|

## 7. 不做的事

| 项目 | 原因 |
|------|------|
| 不新增 Capability 枚举 | 图片理解是 Chat 的 input modality，不是独立能力 |
| 不新增配置块 | 现有 provider + routing 配置已足够 |
| 不修改持久化历史 | 所有变换在 clone 层进行 |
| 不在辅助翻译时发历史消息 | 避免协议兼容性问题，图片描述是自包含的 |
| 不保留图片 URL | LLM 无法访问 URL，且多数 CDN 链接会过期 |
| 不新增独立 trait | 辅助翻译就是一次 Chat 调用 |

## 8. 后续扩展

### 8.1 音频支持（Phase 2）

同样的模式适用于音频：

```rust
let aux_audio = if !primary_config.supports_audio_input() {
    runtime.providers.find_chat_model_with_modality(Modality::Audio)
} else {
    None
};
```

当轮音频通过支持音频的 Chat 模型转录为文本，注入消息。

### 8.2 视频支持（Phase 3）

同上，查找 `Modality::Video`。

### 8.3 可选优化

- **描述缓存**：对同一图片的翻译结果缓存（sha256 URL），避免重复调用（Hermes 用 `_anthropic_image_fallback_cache` 实现了这点）
- **图片压缩**：大图先压缩再发给辅助模型（Hermes 有 `_try_shrink_image_parts_in_messages`）
- **并行翻译**：多张图片并行调用辅助模型
- **流式翻译**：辅助模型也用流式请求，减少感知延迟

## 9. 测试要点

1. **主模型支持图片**：验证现有流程不受影响（回归测试）
2. **主模型不支持图片 + 有辅助模型**：验证图片被翻译为文本描述
3. **主模型不支持图片 + 无辅助模型**：验证图片被替换为占位符
4. **历史中含图片 + 切换到非视觉模型**：验证历史图片被替换为 `[image]`
5. **持久化验证**：验证所有变换不影响 `history.jsonl`
6. **Compaction 回归**：验证 `strip_images()` 行为不变
7. **多图场景**：多张图片同时翻译
8. **辅助模型失败**：验证 graceful degradation

## 10. 变更清单

| 文件 | 变更类型 | 说明 |
|------|---------|------|
| `src/providers/capability.rs` | 新增方法 | `supports_audio_input()`, `supports_video_input()`, `supports_input()` |
| `src/providers/provider_registry.rs` | 新增方法 | `find_chat_model_with_modality()` |
| `src/registry/mod.rs` | 新增实现 | `find_chat_model_with_modality()` 的 Registry 实现 |
| `src/agents/modality_adapter.rs` | **新增文件** | 模态适配核心逻辑 |
| `src/agents/agent.rs` | 修改 | `run()` 中集成 `adapt_messages()` |
| `src/agents/mod.rs` | 修改 | 声明 `mod modality_adapter` |
