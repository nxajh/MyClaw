# MyClaw 多模态辅助翻译方案

> RFC v2 · 2026-06-03
>
> **v2 修订（基于代码评审）**：
> - 修正辅助翻译的流式消费（`ChatProvider::chat()` 返回 `BoxStream`，须经 `ChatResponse::from_stream()` 聚合；`stream` 恒为 `true`）
> - 引入**描述缓存**保证多轮追问正确性（历史图片复用描述而非一律降级为 `[image]`）
> - 辅助模型选择改为**显式配置优先、自动发现兜底**（`[routing.image_aux]`），避免成本不可控
> - 模块按 **modality 泛型化**（`ModalitySpec`），audio/video 不再重复核心逻辑
> - `find_chat_model_with_modality` 基于现有公开 trait 方法实现，不触碰私有字段
> - 移除冗余 `supports_audio_input/video_input`，统一为 `supports_input(modality)`；并行翻译纳入核心

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

### 3.3 辅助翻译 = 临时 Chat 调用（流式）

不需要独立的 `ImageUnderstandingProvider` trait。辅助翻译就是一次普通的 Chat API 调用：

```
辅助调用 = 单条 user 消息（图片 + "请描述这张图片"）→ Chat API → 文本描述
```

**关键：`ChatProvider::chat()` 是流式接口**，返回 `BoxStream<StreamEvent>`，不是直接返回文本。
非流式调用方必须用 `ChatResponse::from_stream()` 聚合（见 `capability_chat.rs:246`）：

```rust
let stream = provider.chat(req)?;                       // 启动流
let resp = ChatResponse::from_stream(stream).await?;    // 聚合为完整文本
let description = resp.text;
```

同时 `ChatRequest.stream` 字段约定恒为 `true`（"always true; caller must not set false"），
辅助调用也不例外。

**不发历史消息**，因为：
- 避免不同协议间的 Thinking blocks / tool_calls 兼容性问题
- 图片描述是自包含的，不需要对话上下文
- 减少延迟和 token 消耗

### 3.4 描述缓存：保证多轮正确性（核心）

> **这不是可选优化，而是多轮对话的正确性前提。**

辅助翻译的结果**不持久化**（持久化 history 必须保留原始 `ImageUrl`，以便切回视觉模型时仍能原生使用）。
若历史图片一律简单替换为 `[image]`，会丢失追问能力：

```
Turn N:   用户发图 → 翻译为 "[图片描述]: 穿红衬衫的人..."（仅存在于 clone 层）
Turn N+1: 用户「刚才那张图里衬衫什么颜色?」
          → 该图已成历史 → 若替换为 "[image]"
          → 主模型只看到 "[image]"，无法回答 ❌
```

**解决方案：按图片内容指纹缓存描述文本**，当轮写入、历史复用：

```
key   = sha256(image_url | image_b64)
value = 翻译得到的文本描述

当轮图片：查缓存 → miss 则调用辅助模型并写入缓存
历史图片：查缓存 → hit 则复用 "[图片描述]: ...";miss 才降级为 "[image]"
```

这样同时满足：
- **持久化不变** —— history 仍存原始 `ImageUrl`
- **多轮正确** —— 历史图片复用已生成的描述，追问可答
- **成本可控** —— 每张图只翻译一次（同一图片跨轮 / 多次出现都命中缓存）
- **可切回视觉模型** —— history 中 `ImageUrl` 原样保留

缓存作用域：进程级 LRU（key 为内容 sha256，与会话无关，天然可跨会话复用）。
Hermes 的 `_anthropic_image_fallback_cache` 是同一思路。

### 3.5 消息变换发生在 clone 层

当轮图片来自 `session.last_message.image_urls / image_base64` 快照，**在 messages 层注入**，
不写回持久化 history。历史图片才以 `ContentPart::ImageUrl/ImageB64` 存在于 clone 出的 messages 中。
这让"当轮 vs 历史"的区分天然成立，无需启发式猜测。

```
Layer 1: 持久化历史 (history.jsonl) — 永不修改
         ↓ session.history.iter().cloned()
Layer 2: messages: Vec<ChatMessage> — clone，可修改
         ↓ sanitize_history()                    [已有]
         ↓ adapt_history_media()  历史图片→缓存复用/占位符   [新增]
         ↓ 主模型支持？
         │   ├─ 是：附加 ImageUrl/B64 parts（当轮原生）      [已有]
         │   └─ 否：adapt_pending_media() 当轮图片→辅助翻译注入 [新增]
Layer 3: 协议渲染 render_*_body()                [已有]
```

和 Hermes 的 `copy.deepcopy(api_messages)` 是同一思路。

## 4. 详细设计

### 4.1 能力查询方法

`supports_image_input()` **已存在**（`capability.rs:134`）。只需补充一个泛型方法，
音频/视频复用它，避免为每种 modality 写一个布尔函数：

```rust
// src/providers/capability.rs

impl ChatModelConfig {
    // 已有：pub fn supports_image_input(&self) -> bool { ... }

    /// Whether the model supports the given input modality.
    pub fn supports_input(&self, modality: Modality) -> bool {
        self.input.contains(&modality)
    }
}
```

> 不再新增 `supports_audio_input()` / `supports_video_input()` —— 它们只是
> `supports_input(Modality::Audio/Video)` 的别名，徒增 API 表面积。`supports_image_input()`
> 因已被 `agent.rs` 引用而保留。

### 4.2 新增方法：`find_chat_model_with_modality()`

选择辅助模型时**显式配置优先、自动发现兜底**，避免「零配置」自动挑中昂贵模型导致成本/延迟不可控。
优先级：

1. **显式配置** `[routing.<modality>_aux]`（可选，见 §4.6）—— 用户指定专用辅助模型
2. **chat 路由链**中支持该 modality 的模型 —— 复用主链已注册的视觉模型
3. **任意已注册** chat 模型中支持该 modality 的 —— 最后兜底

实现完全基于 `ProviderRegistry` 的**现有公开方法**，不触碰私有 `chat_providers` 字段：

```rust
// src/registry/mod.rs — impl ProviderRegistry for Registry

/// Find a registered chat model that supports the given input modality.
/// Prefers an explicitly-configured auxiliary model, then the chat routing
/// chain, then any registered chat model. Returns None if none qualifies.
fn find_chat_model_with_modality(
    &self,
    modality: Modality,
) -> Option<(Arc<dyn ChatProvider>, String)> {
    // 候选 model_id 列表，按优先级排列
    let mut candidates: Vec<String> = Vec::new();
    // 1. 显式配置的辅助模型（如有）
    if let Some(id) = self.aux_model_for(modality) {
        candidates.push(id);
    }
    // 2. chat 路由链
    candidates.extend(self.get_chat_routing_models());
    // 3. 所有已注册 chat 模型
    for summary in self.get_all_provider_summaries() {
        candidates.extend(summary.chat_models);
    }

    for model_id in candidates {
        let supports = self
            .get_chat_model_config(&model_id)
            .map(|cfg| cfg.supports_input(modality))
            .unwrap_or(false);
        if supports {
            // get_chat_provider_by_model 是已有公开方法，按精确 model_id 取 provider
            if let Some(found) = self.get_chat_provider_by_model(&model_id) {
                return Some(found);
            }
        }
    }
    None
}
```

> `aux_model_for(modality)` 是 Registry 内部小助手，从 §4.6 的可选路由配置读取；
> 无配置时返回 `None`，逻辑自然落到优先级 2/3。
> 候选列表可能含重复 id，但首个命中即返回，重复无害；如需严格可加 `seen` 去重。

### 4.3 新增模块：`modality_adapter`

模块按 **modality 驱动**设计：每种模态由一个 `ModalitySpec` 描述（如何匹配、用什么 prompt、
占位符文案），核心逻辑只写一遍，audio/video 只需新增 spec，**不重复检测/替换/翻译代码**。

#### 4.3.1 模态描述与媒体指纹

```rust
// src/agents/modality_adapter.rs

//! Modality adaptation layer — translates non-text modalities to text
//! when the primary chat model does not support them natively.
//!
//! Operates on cloned messages only (never mutates persistent history).
//! Translation results are cached by content fingerprint so historical
//! media can reuse a description instead of degrading to a placeholder.

use crate::providers::capability::Modality;
use crate::providers::capability_chat::{
    ChatMessage, ChatProvider, ChatRequest, ChatResponse, ContentPart,
};
use sha2::{Digest, Sha256};
use std::sync::Arc;

/// Static description of how to adapt one modality.
struct ModalitySpec {
    modality: Modality,
    /// Prompt sent to the auxiliary model.
    prompt: &'static str,
    /// Label for the injected description, e.g. "图片" / "音频" / "视频".
    label: &'static str,
    /// Placeholder when no description is available, e.g. "[image]".
    placeholder: &'static str,
}

const IMAGE_SPEC: ModalitySpec = ModalitySpec {
    modality: Modality::Image,
    prompt: "Describe this image in detail, including any text, objects, \
             layout, and notable visual information.",
    label: "图片",
    placeholder: "[image]",
};
// Phase 2/3: const AUDIO_SPEC / VIDEO_SPEC — same struct, different fields.

/// Whether a part carries media of the given modality.
fn part_matches(part: &ContentPart, modality: Modality) -> bool {
    match modality {
        Modality::Image => matches!(
            part,
            ContentPart::ImageUrl { .. } | ContentPart::ImageB64 { .. }
        ),
        // Phase 2/3: Audio / Video parts once ContentPart gains them.
        _ => false,
    }
}

/// Content fingerprint used as the description-cache key.
/// URL images key on the URL; base64 images key on the payload bytes.
fn fingerprint(part: &ContentPart) -> Option<String> {
    let seed = match part {
        ContentPart::ImageUrl { url, .. } => url.as_str(),
        ContentPart::ImageB64 { b64_json, .. } => b64_json.as_str(),
        _ => return None,
    };
    Some(format!("{:x}", Sha256::digest(seed.as_bytes())))
}
```

#### 4.3.2 描述缓存

```rust
/// Process-wide LRU cache: content fingerprint → text description.
/// Shared via the runtime so it survives across turns and sessions.
pub trait DescriptionCache: Send + Sync {
    fn get(&self, key: &str) -> Option<String>;
    fn put(&self, key: String, value: String);
}
// Concrete impl: a Mutex<LruCache<String, String>> living on AgentRuntime.
```

#### 4.3.3 辅助翻译（正确的流式消费）

```rust
/// Translate one media part to text using an auxiliary model.
/// Returns the cached description on hit; otherwise performs a single,
/// self-contained (history-free) streaming chat call and caches the result.
async fn translate_part(
    provider: &dyn ChatProvider,
    model_id: &str,
    part: &ContentPart,
    spec: &ModalitySpec,
    cache: &dyn DescriptionCache,
) -> anyhow::Result<String> {
    if let Some(key) = fingerprint(part) {
        if let Some(hit) = cache.get(&key) {
            return Ok(hit);
        }
    }

    let user_msg = ChatMessage {
        role: "user".into(),
        parts: vec![part.clone(), ContentPart::Text { text: spec.prompt.into() }],
        name: None,
        tool_call_id: None,
        tool_calls: None,
        is_error: None,
    };

    let messages = [user_msg];
    let req = ChatRequest {
        model: model_id,
        messages: &messages,
        temperature: Some(0.3),
        max_tokens: Some(1024),
        thinking: None,
        stop: None,
        seed: None,
        tools: None,
        stream: true,            // 约定恒为 true
    };

    // chat() 返回 BoxStream，必须聚合为完整响应
    let stream = provider.chat(req)?;
    let resp = ChatResponse::from_stream(stream).await?;
    let text = resp.text;

    if let Some(key) = fingerprint(part) {
        cache.put(key, text.clone());
    }
    Ok(text)
}
```

#### 4.3.4 历史媒体：缓存复用 / 占位符

```rust
/// Replace historical media parts (everything except the current turn,
/// which is handled by `adapt_pending_media`). Reuses a cached description
/// when available so follow-up questions still work; otherwise degrades to
/// the placeholder. Never calls the auxiliary model (no new translation for
/// stale context — cache hits are free, misses are not worth a round-trip).
pub fn adapt_history_media(
    messages: &mut [ChatMessage],
    spec: &ModalitySpec,
    cache: &dyn DescriptionCache,
) {
    for msg in messages.iter_mut() {
        for part in msg.parts.iter_mut() {
            if !part_matches(part, spec.modality) {
                continue;
            }
            let replacement = fingerprint(part)
                .and_then(|k| cache.get(&k))
                .map(|desc| format!("[{}描述]: {}", spec.label, desc))
                .unwrap_or_else(|| spec.placeholder.to_string());
            *part = ContentPart::Text { text: replacement };
        }
    }
}
```

#### 4.3.5 当轮媒体：翻译并注入

当轮图片由 `agent.rs` 从 `session.last_message` 快照提供（见 §4.4），尚未进入 `parts`，
所以这里直接产出要注入的文本，由调用方 push 到最后一条 user 消息：

```rust
/// Translate current-turn media to a single text block to inject into the
/// last user message. Parts are translated in parallel. Falls back to a
/// graceful placeholder when no auxiliary model is available or a call fails.
pub async fn adapt_pending_media(
    pending: &[ContentPart],
    spec: &ModalitySpec,
    aux: Option<(&Arc<dyn ChatProvider>, &str)>,
    cache: &dyn DescriptionCache,
) -> Option<ContentPart> {
    let media: Vec<&ContentPart> =
        pending.iter().filter(|p| part_matches(p, spec.modality)).collect();
    if media.is_empty() {
        return None;
    }

    let Some((provider, model_id)) = aux else {
        // No auxiliary model: single explicit placeholder.
        return Some(ContentPart::Text {
            text: format!("[{} — no {:?} model available]", spec.placeholder, spec.modality),
        });
    };

    // Parallel translation — multiple images don't serialize latency.
    let futs = media.iter().map(|part| {
        translate_part(provider.as_ref(), model_id, part, spec, cache)
    });
    let results = futures_util::future::join_all(futs).await;

    let descriptions: Vec<String> = results
        .into_iter()
        .map(|r| match r {
            Ok(desc) => desc,
            Err(e) => {
                tracing::warn!(err = %e, modality = ?spec.modality, "auxiliary translation failed");
                "[translation failed]".to_string()
            }
        })
        .collect();

    let text = if descriptions.len() == 1 {
        format!("[{}描述]: {}", spec.label, descriptions[0])
    } else {
        descriptions
            .iter()
            .enumerate()
            .map(|(i, d)| format!("[{}{}描述]: {}", spec.label, i + 1, d))
            .collect::<Vec<_>>()
            .join("\n")
    };
    Some(ContentPart::Text { text })
}
```

要点小结（相对旧稿的修正）：
- **流式消费**：`provider.chat(req)?` → `ChatResponse::from_stream(stream).await?`，`stream: true`
- **缓存复用**：历史图片命中缓存即复用描述，多轮追问不再失效
- **去掉 `unreachable!()`**：`part_matches` 谓词 + `filter`，非媒体 part 自然跳过
- **去掉 drain/retain 拼接**：当轮直接产出一个 `Text` part 供注入，不再「插空再清理」
- **modality 泛型**：核心逻辑只写一遍，audio/video 仅加 `ModalitySpec` + `part_matches` 分支
- **并行翻译**：多图 `join_all`，不串行累加延迟

### 4.4 修改 `agent.rs`：集成模态适配

集成点与现有代码的两个事实对齐：
- **历史图片**在 clone 出的 `messages` 的 `parts` 里 → 用 `adapt_history_media` 处理（缓存复用/占位符）
- **当轮图片**来自 `session.last_message.image_urls / image_base64` 快照（`agent.rs:126-133`），
  主模型支持时附加原生 parts（现有逻辑），不支持时改走 `adapt_pending_media` 注入文本

```rust
// src/agents/agent.rs — run() 方法中

// 现有代码：构建 messages
let mut messages: Vec<ChatMessage> = std::iter::once(system_msg.clone())
    .chain(session.history.iter().cloned())
    .collect();
crate::agents::session::sanitize_history(&mut messages);

let cache = runtime.description_cache.as_ref();   // §4.7 挂在 AgentRuntime 上

// ===== 新增 1：历史图片适配（无论主模型是否支持，统一规范化历史）=====
// 仅当主模型不支持图片时才需要把历史 ImageUrl 文本化；支持时保持原样。
if !model_supports_images {
    modality_adapter::adapt_history_media(
        &mut messages,
        &modality_adapter::IMAGE_SPEC,
        cache,
    );
}
// ===== 结束 1 =====
```

当轮图片的分叉发生在原有的「图片附加」处：

```rust
// 现有代码：图片附加（仅在主模型支持时）—— 保持不变
if !images_attached {
    if model_supports_images {
        // ... 现有逻辑不变：把 pending ImageUrl/B64 push 到最后一条 user 消息
    } else {
        // ===== 新增 2：主模型不支持 → 当轮图片辅助翻译注入 =====
        let aux = runtime.providers.find_chat_model_with_modality(Modality::Image);
        let aux_ref = aux.as_ref().map(|(p, id)| (p, id.as_str()));

        // 把 pending 快照拼成 ContentPart 列表供翻译
        let pending_parts = build_pending_parts(&pending_image_urls, &pending_image_b64);
        if let Some(injected) = modality_adapter::adapt_pending_media(
            &pending_parts,
            &modality_adapter::IMAGE_SPEC,
            aux_ref,
            cache,
        ).await {
            if let Some(last_user) = messages.iter_mut().rev().find(|m| m.role == "user") {
                last_user.parts.insert(0, injected);
            }
        }
        // ===== 结束 2 =====
    }
}
images_attached = true;
```

> `build_pending_parts` 把 `Vec<String>` URL / base64 还原为 `ContentPart::ImageUrl/ImageB64`
> （`media_type: None`, `detail: Auto`），与现有附加逻辑构造的 part 完全一致，从而 fingerprint
> 一致、缓存可跨「视觉模型轮」与「非视觉模型轮」命中。

### 4.5 ProviderRegistry trait 扩展

```rust
// src/providers/provider_registry.rs — 新增一个 trait 方法

/// Find a registered chat model that supports the given input modality.
/// Default-implemented in terms of existing trait methods
/// (get_chat_routing_models / get_all_provider_summaries /
///  get_chat_model_config / get_chat_provider_by_model), so the Registry
/// impl needs no access to private fields.
fn find_chat_model_with_modality(
    &self,
    modality: Modality,
) -> Option<(Arc<dyn ChatProvider>, String)>;
```

> 可作为 **trait 默认方法**实现（§4.2 的逻辑全部基于已有公开方法），各实现者零额外样板。
> 仅显式 aux 配置查询 `aux_model_for` 需要具体实现读取 §4.6 的配置。

### 4.6 可选配置：`[routing.<modality>_aux]`

辅助模型选择默认走自动发现（§4.2 优先级 2/3），**无需任何配置即可工作**。
但允许用户显式指定，以控制成本/延迟/质量：

```toml
# 可选：为图片理解指定专用辅助模型（不配则自动发现）
[routing.image_aux]
models = ["gpt-4o-mini"]

# Phase 2/3：
# [routing.audio_aux]
# models = ["gpt-4o-audio"]
```

复用现有 `RoutingConfig`（`HashMap<String, RouteEntry>`）结构，新增 `image_aux` / `audio_aux` /
`video_aux` 几个约定键即可，无需新增配置类型。`aux_model_for(modality)` 读取对应键的首个 model。

### 4.7 描述缓存挂载点

`DescriptionCache`（§4.3.2）作为单例挂在 `AgentRuntime` 上，与 `context_engine`、`loop_breaker`
等共享单例同级：

```rust
// src/agents/runtime.rs（AgentRuntime 定义处）
pub description_cache: Arc<dyn DescriptionCache>,
```

进程级 LRU（建议容量数百条，value 为文本描述，内存占用可忽略），key 为内容 sha256，
天然跨会话复用，无需失效逻辑（同一图片内容描述恒定）。

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
adapt_history_media():  历史 messages 中的 ImageUrl
  ├─ 缓存命中 → "[图片描述]: ..."（复用，多轮追问可答）
  └─ 缓存未命中 → "[image]"
adapt_pending_media():  当轮新图片（来自 last_message 快照）
  ├─ 找到辅助视觉模型（[routing.image_aux] 优先，否则自动发现）
  ├─ 查缓存 → miss 则流式 Chat 调用 from_stream() → 写缓存
  ├─ 多图并行 join_all
  └─ 注入最后一条 user 消息: "[图片描述]: 一张包含...的图片"
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

Turn 2: 用户切换到 DeepSeek V3（不支持视觉），并追问 history[0] 的图片
  messages = clone(history)
  adapt_history_media():
    history[0] ImageUrl → 查缓存
      ├─ Turn 1 用视觉模型，未经辅助翻译 → 缓存 miss → "[image]"
      └─ 若 Turn 1 也是非视觉模型并翻译过 → 缓存 hit → "[图片描述]: ..."（追问可答）
  持久化 history 不变，ImageUrl 仍在 → 随时可切回视觉模型原生使用
```

> **多轮正确性的边界**：仅当某图片在「非视觉模型轮」被翻译过、描述进了缓存，后续追问才能复用。
> 若图片只在「视觉模型轮」出现过（从未辅助翻译），切到非视觉模型追问时缓存为空，仍降级为 `[image]`
> —— 此时也无更优解（从未产生过描述），行为可预期。

## 6. 处理策略总结

| 内容类型 | 场景 | 处理方式 |
|---------|------|---------|
| 当轮新图片 | 主模型支持 | 原生附加 ImageUrl parts |
| 当轮新图片 | 主模型不支持 | 辅助模型翻译（并行 + 写缓存）→ 文本描述注入 |
| 当轮新图片 | 无辅助模型 | `"[image — no Image model available]"` |
| 历史中的图片 | 非视觉模型 + 缓存命中 | `"[图片描述]: ..."`（复用，多轮可答） |
| 历史中的图片 | 非视觉模型 + 缓存未命中 | `"[image]"` |
| 历史中的图片 | 发给视觉模型 | 保持原样 |
| Compaction | 所有图片 | `"[image]"`（已有逻辑）|

## 7. 不做的事

| 项目 | 原因 |
|------|------|
| 不新增 Capability 枚举 | 图片理解是 Chat 的 input modality，不是独立能力 |
| 不强制新增配置块 | 自动发现默认即可工作；`[routing.image_aux]` 仅为可选的显式控制 |
| 不修改持久化历史 | 所有变换在 clone 层进行 |
| 不在辅助翻译时发历史消息 | 避免协议兼容性问题，图片描述是自包含的 |
| 不保留图片 URL | LLM 无法访问 URL，且多数 CDN 链接会过期 |
| 不新增独立 trait | 辅助翻译就是一次（流式）Chat 调用 |
| 不为每个 modality 写一个 `supports_*` | 用泛型 `supports_input(modality)`，避免 API 膨胀 |
| 不对历史媒体重新调用辅助模型 | 仅复用缓存；为过期上下文重新翻译不值一次往返 |

## 8. 后续扩展

### 8.1 音频支持（Phase 2）

同样的模式适用于音频，**只需新增一个 `ModalitySpec` 和 `part_matches` 分支**，核心逻辑零改动：

```rust
const AUDIO_SPEC: ModalitySpec = ModalitySpec {
    modality: Modality::Audio,
    prompt: "Transcribe this audio. Include speaker turns if discernible.",
    label: "音频",
    placeholder: "[audio]",
};

// agent.rs：与图片完全对称
if !primary_config.supports_input(Modality::Audio) {
    let aux_audio = runtime.providers.find_chat_model_with_modality(Modality::Audio);
    // adapt_history_media(&mut messages, &AUDIO_SPEC, cache);
    // adapt_pending_media(&pending_audio, &AUDIO_SPEC, aux_audio_ref, cache);
}
```

前提：`ContentPart` 需先新增音频变体（如 `AudioB64 { .. }`），`part_matches` 增加对应分支。

### 8.2 视频支持（Phase 3）

同上，查找 `Modality::Video`。

### 8.3 已纳入核心 / 仍可选

**已纳入本设计核心**（不再是「可选」）：
- **描述缓存**：sha256(内容) → 描述，保证多轮正确性（§3.4 / §4.3.2）
- **并行翻译**：多图 `join_all`（§4.3.5）
- **流式调用**：辅助调用本就走 `ChatProvider::chat()` 流式接口（§3.3）

**仍属可选优化**：
- **图片压缩**：大图先压缩再发给辅助模型（Hermes 有 `_try_shrink_image_parts_in_messages`），降低辅助调用成本
- **缓存持久化**：进程级 LRU 重启即失。可选落盘（sqlite/jsonl），跨重启复用描述
- **描述质量分级**：按下游用途调节 prompt 详略（缩略图 vs 全文 OCR）

## 9. 测试要点

1. **主模型支持图片**：验证现有流程不受影响（回归测试）
2. **主模型不支持图片 + 有辅助模型**：验证图片被翻译为文本描述，且 `stream:true` + `from_stream()` 聚合正确
3. **主模型不支持图片 + 无辅助模型**：验证图片被替换为占位符
4. **历史中含图片 + 切换到非视觉模型（缓存未命中）**：验证历史图片被替换为 `[image]`
5. **多轮追问（缓存命中）**：Turn N 非视觉模型翻译某图 → Turn N+1 追问该图 → 验证历史图片复用 `[图片描述]: ...` 而非 `[image]`
6. **缓存去重**：同一图片（相同 URL / b64）多次出现 / 跨会话 → 验证辅助模型仅被调用一次（fingerprint 命中）
7. **持久化验证**：验证所有变换不影响 `history.jsonl`，`ImageUrl` 原样保留
8. **Compaction 回归**：验证 `strip_images()` 行为不变
9. **多图并行**：多张图片 `join_all` 并行翻译，结果顺序与注入文案编号一致
10. **辅助模型失败**：单图失败 → `[translation failed]`，不影响其他图片（graceful degradation）
11. **显式 aux 配置**：`[routing.image_aux]` 指定模型时，验证优先于自动发现选中

## 10. 变更清单

| 文件 | 变更类型 | 说明 |
|------|---------|------|
| `src/providers/capability.rs` | 新增方法 | 仅 `supports_input(modality)`（`supports_image_input()` 已存在） |
| `src/providers/provider_registry.rs` | 新增方法 | `find_chat_model_with_modality()`（可为 trait 默认方法） |
| `src/registry/mod.rs` | 新增实现 | `aux_model_for(modality)`（读 `[routing.*_aux]`）；如不用默认方法则实现 `find_chat_model_with_modality()` |
| `src/config/routing.rs` | 可选新增键 | 约定键 `image_aux` / `audio_aux` / `video_aux`（复用 `RouteEntry`） |
| `src/agents/modality_adapter.rs` | **新增文件** | `ModalitySpec` / `DescriptionCache` / `translate_part`（流式）/ `adapt_history_media` / `adapt_pending_media` |
| `src/agents/runtime.rs` | 新增字段 | `description_cache: Arc<dyn DescriptionCache>` 单例 |
| `src/agents/agent.rs` | 修改 | `run()` 集成 `adapt_history_media` + 当轮分叉 `adapt_pending_media` |
| `src/agents/mod.rs` | 修改 | 声明 `mod modality_adapter` |
| `Cargo.toml` | 依赖 | `sha2`（指纹）、`lru`（缓存）；`futures-util` 已在用 |
