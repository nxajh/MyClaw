# MyClaw 多模态辅助翻译方案

> RFC v3 · 2026-06-03
>
> **v3 修订（修正 v2 的数据流前提）**：
> - **当轮图片持久化进 `session.history`**：v2 的「clone 历史 → 适配媒体 → 缓存复用描述」整套设计，**隐含前提是历史里真有图片 parts**。但现状代码 `Session::add_user()` 只把**文本**写入 history，原始图片仅短暂存在于 `session.last_message`，turn 结束即弃（见 §1.1 现状修正）。本版把当轮图片作为 `ContentPart::ImageUrl/ImageB64` 一并持久化进 history，使 v2 的 clone 层与缓存方案真正成立。
> - 由此**简化** `agent.rs`：不再「从 `last_message` 快照临时拼回图片」。history 自带图片，`messages = clone(history)` 天然携带，当轮 vs 历史由**位置**（最后一条 user 消息 vs 更早消息）区分，无需启发式。
> - 持久化图片**优于 OpenClaw 纯文本模式**：OpenClaw 把图片转文本后丢弃原图，切回视觉模型即失去原始内容；MyClaw 保留原图 + 缓存描述，做到「视觉模型用原图、非视觉模型用缓存描述」两全。
> - **blob 外置纳入 v1**（§11.8）：大 b64 字节落盘到 `sessions/{id}/blobs/`，`history.jsonl` 行内只存 `ImageRef { hash }`，读取时 hydrate 回 `ImageB64`。`ImageRef` 只存在于磁盘，内存始终是 `ImageB64`，故渲染/adapt/缓存层零改动。与图片持久化是同一条写路径的两步。
> - 新增 **§11 持久化体积与已知风险**：blob 外置、URL 过期、与 compaction `strip_images`/GC 的协作、token 计账、prompt injection、测试策略——前几版口头列举的待细化点在此落定。
>
> **v2 修订（基于代码评审）**：
> - 修正辅助翻译的流式消费（`ChatProvider::chat()` 返回 `BoxStream`，须经 `ChatResponse::from_stream()` 聚合；`stream` 恒为 `true`）
> - 引入**描述缓存**保证多轮追问正确性（历史图片复用描述而非一律降级为 `[image]`）
> - 辅助模型选择**只在用户显式声明的集合内**筛选（`[routing.image_aux]` 覆盖 → `[routing.chat]` 链 → 占位符降级），不做全局自动发现，与 MyClaw「显式路由」哲学一致
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
- `ContentPart` 枚举支持 `ImageUrl / ImageB64 / Thinking` 等多种内容类型（且 `Serialize/Deserialize`，可落盘）
- `context_engine.rs` 的 compaction 流程有 `strip_images()` 将图片替换为 `[image]`

> **关键现状修正（v3）**：当轮图片**目前不进 history**。`process_turn`（`session_context.rs:119/180`）
> 先 `record_inbound(msg)` 把完整 `ChannelMessage`（含 `image_urls/image_base64`）存进
> `session.last_message`，再 `add_user(text)` —— 而 `add_user`（`session/types.rs:203`）只
> `push(ChatMessage::user_text(text))`，**只存文本**。视觉模型靠 `agent.rs` 从 `last_message`
> **快照**临时把图片拼回 messages；turn 结束后 history 里那条 user 消息**没有图片**。
> 因此 v2 设想的「历史图片以 `ImageUrl` 存在于 clone 出的 messages」在当前代码下**并不成立**——
> 历史里根本没有图片可供缓存复用或切回视觉模型原生使用。v3 修正这一数据流。

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

运行时从**已配置的 `[routing.chat]` 链**中查找支持特定 modality 的模型即可（§4.2）——
候选集是用户显式声明的，不扫描全局，零额外配置。

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

辅助翻译的**结果**（文本描述）不写回 history —— history 持久化的是**原始 `ImageUrl/ImageB64`**
（v3 起当轮图片真正进 history，见 §3.6），以便切回视觉模型时仍能原生使用。描述只活在缓存与 clone 层。
若历史图片在 clone 层一律简单替换为 `[image]`，会丢失追问能力：

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

v3 起，**当轮与历史图片都以 `ContentPart::ImageUrl/ImageB64` 驻留在 history**（§3.6），
故 `messages = clone(history)` 天然携带全部图片。所有媒体变换只在这份 clone 上进行，
**永不写回持久化 history**。"当轮 vs 历史"由**位置**区分——最后一条 user 消息是当轮、更早的是历史
——无需快照拼接，也无需启发式猜测。

```
Layer 1: 持久化历史 (history.jsonl) — 永不修改；含原始 ImageUrl/B64（v3）
         ↓ session.history.iter().cloned()
Layer 2: messages: Vec<ChatMessage> — clone，可修改
         ↓ sanitize_history()                          [已有]
         ↓ 主模型支持图片？
         │   ├─ 是：保持 ImageUrl/B64 parts 原样（当轮+历史均原生）   [已有/简化]
         │   └─ 否：
         │        adapt_history_media()   非末条 user 的图片→缓存复用/占位符  [新增]
         │        adapt_pending_media()    末条 user 的图片→辅助翻译注入       [新增]
Layer 3: 协议渲染 render_*_body()                       [已有]
```

和 Hermes 的 `copy.deepcopy(api_messages)` 是同一思路。

### 3.6 当轮图片持久化进 history（v3 核心修正）

> 把图片本身存下来，而非只在内存里转一次文本。

**改动点**：`Session::add_user` 增加一条携带媒体的入口（如 `add_user_with_media`），
`process_turn` 在记录当轮 user 消息时，把 `last_message` 的 `image_urls/image_base64`
作为 `ContentPart::ImageUrl/ImageB64` 一并放进那条 user 消息的 `parts`。`persist_hook`
（`session_context.rs:181`）随即把这条**含图片的** `ChatMessage` 写入 `history.jsonl`
（`ContentPart` 已是 `Serialize`，天然可落盘）。

> **写入即外置（v1）**：为避免大 b64 撑爆 `history.jsonl`，落盘这一步由 `JsonFileBackend`
> 把超阈值的 `ImageB64` 字节外置到 `blobs/`、行内只留 `ImageRef { hash }`，读取时再 hydrate
> 回 `ImageB64`。`ImageRef` **只存在于磁盘**，内存中 `session.history` 始终是 `ImageB64`，
> 故渲染/adapt/缓存层无感。持久化（本节）与外置（§11.8）是同一条写路径上的两步，v1 一并实现。

**为什么这是正确修法，而不仅是优化**：

| 维度 | 现状（图片不进 history） | v3（图片持久化） |
|------|------------------------|-----------------|
| v2 的 clone 层/缓存方案 | 前提不成立（历史无图） | **成立**——历史真有图可复用/原生用 |
| 切回视觉模型 | 历史图已丢，无法原生使用 | 原图在 history，**原生使用** |
| 多轮追问（非视觉模型） | 历史无图，无从翻译 | 历史图在，**缓存复用描述**可答 |
| agent.rs 复杂度 | 需从 `last_message` 快照拼回 | **简化**：clone 即带图，删快照逻辑 |
| 对比 OpenClaw 纯文本模式 | —— | **更优**：OpenClaw 转文本即弃原图，切回视觉模型无原图；MyClaw 两全 |

代价（持久化体积、URL 过期）与缓解见 **§11**。

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

**设计原则：候选集必须落在用户已显式授权的范围内**，与 MyClaw 一以贯之的「显式路由」
哲学保持一致（`get_chat_provider` 在 `[routing.chat]` 缺失时直接 error，全代码库无任何自动发现）。
因此辅助模型选择**不扫描全局已注册模型**，只在用户显式声明的集合里筛：

1. **显式覆盖** `[routing.image_aux]`（可选，见 §4.6）—— 多视觉模型时指定最优辅助模型
2. **默认**：`[routing.chat]` 链中支持该 modality 的模型 —— 复用用户已声明的 `models = [...]`
3. 都没有 → 返回 `None` → **优雅降级为占位符**（辅助翻译是增强非必需，不 error，也不全局扫描）

> 不做「从所有已注册模型里自动挑一个」：那会选到用户未放进路由链的模型（越权）、
> 多模型时不可预测、且引入代码库中唯一的隐式魔法。零额外配置由「复用已有 chat 链」达成，
> 而非全局发现。

实现完全基于 `ProviderRegistry` 的**现有公开方法**，不触碰私有 `chat_providers` 字段：

```rust
// src/registry/mod.rs — impl ProviderRegistry for Registry

/// Find a chat model that supports the given input modality, searching only
/// the user-declared set: an explicit `[routing.<modality>_aux]` override
/// first, then the `[routing.chat]` fallback chain. Returns None (caller
/// degrades to a placeholder) when neither yields a capable model — we never
/// scan all registered models, matching MyClaw's explicit-routing philosophy.
fn find_chat_model_with_modality(
    &self,
    modality: Modality,
) -> Option<(Arc<dyn ChatProvider>, String)> {
    // 候选 model_id，按优先级排列；二者皆为用户显式声明的集合
    let mut candidates: Vec<String> = Vec::new();
    // 1. 显式 aux 覆盖（如配置）
    if let Some(id) = self.aux_model_for(modality) {
        candidates.push(id);
    }
    // 2. chat 路由链（用户在 [routing.chat] models = [...] 中显式列出）
    candidates.extend(self.get_chat_routing_models());
    // （刻意不加：全局 get_all_provider_summaries 扫描）

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

> `aux_model_for(modality)` 从 §4.6 的可选配置读取；无配置时返回 `None`，逻辑自然落到优先级 2。

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
/// Replace historical media parts (everything except the current turn at
/// `skip_idx`, which is handled by `adapt_last_turn_media`). Reuses a cached
/// description when available so follow-up questions still work; otherwise
/// degrades to the placeholder. Never calls the auxiliary model (no new
/// translation for stale context — cache hits are free, misses are not worth
/// a round-trip).
pub fn adapt_history_media(
    messages: &mut [ChatMessage],
    spec: &ModalitySpec,
    cache: &dyn DescriptionCache,
    skip_idx: Option<usize>,
) {
    for (i, msg) in messages.iter_mut().enumerate() {
        if Some(i) == skip_idx {
            continue;   // 当轮消息留给 adapt_last_turn_media
        }
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

v3 起当轮图片已作为 parts 驻留在 messages 末条 user 消息（§3.6），调用方把这些媒体 parts
切片传入；本函数产出一个文本 `ContentPart`，由调用方**替换掉**末条消息里的媒体 parts
（而非另存快照再插空）：

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

v3 起图片已驻留 history（§3.6），集成大幅简化为两件事：

- **主模型支持图片**：`messages = clone(history)` 已带全部 `ImageUrl/B64` parts → **保持原样**，
  不再需要从 `last_message` 快照拼回（删除原 `agent.rs:126-133` 那段附加逻辑）。
- **主模型不支持**：对 clone 出的 messages 做媒体适配——**末条 user 消息**的图片是当轮
  （`adapt_pending_media` 翻译注入），**更早**消息的图片是历史（`adapt_history_media`
  缓存复用/占位符）。

```rust
// src/agents/agent.rs — run() 方法中

// 构建 messages：history 已含 ImageUrl/B64 parts（v3）
let mut messages: Vec<ChatMessage> = std::iter::once(system_msg.clone())
    .chain(session.history.iter().cloned())
    .collect();
crate::agents::session::sanitize_history(&mut messages);

if !model_supports_images {
    let cache = runtime.description_cache.as_ref();   // §4.7 挂在 AgentRuntime 上

    // 定位末条 user 消息的下标 = 当轮；其余 user 消息的图片为历史。
    let last_user_idx = messages
        .iter()
        .rposition(|m| m.role == "user");

    // ===== 历史图片：缓存复用 / 占位符（不调用辅助模型）=====
    modality_adapter::adapt_history_media(
        &mut messages,
        &modality_adapter::IMAGE_SPEC,
        cache,
        last_user_idx,   // 跳过此下标，留给 adapt_pending_media
    );

    // ===== 当轮图片：辅助翻译注入 =====
    if let Some(idx) = last_user_idx {
        let aux = runtime.providers.find_chat_model_with_modality(Modality::Image);
        let aux_ref = aux.as_ref().map(|(p, id)| (p, id.as_str()));
        modality_adapter::adapt_last_turn_media(
            &mut messages[idx],
            &modality_adapter::IMAGE_SPEC,
            aux_ref,
            cache,
        ).await;
    }
}
// 主模型支持图片时：什么都不做，原生 parts 直接渲染。
```

> `adapt_history_media` 增加一个 `skip_idx: Option<usize>` 参数跳过当轮消息；
> `adapt_last_turn_media` 是 §4.3.5 `adapt_pending_media` 的就地版本：在传入的那条 user 消息上，
> 收集其媒体 parts → 翻译 → 用产出的文本 part **替换**这些媒体 parts（保留原有 `Text` parts）。
> 因当轮图片本就是 history 里构造的同一个 `ContentPart`，fingerprint 天然一致——缓存可跨
> 「视觉模型轮」与「非视觉模型轮」命中，无需 `build_pending_parts` 重建。

**持久化入口**（`session_context.rs` + `session/types.rs`）：

```rust
// session/types.rs — 新增携带媒体的入口
pub fn add_user_with_media(&mut self, text: String, media: Vec<ContentPart>) {
    let mut parts = Vec::with_capacity(media.len() + 1);
    parts.extend(media);                       // ImageUrl/B64 在前
    parts.push(ContentPart::Text { text });    // 文本在后
    self.history.push(ChatMessage { role: "user".into(), parts, ..Default::default() });
    self.message_ids.push(0);
}

// session_context.rs process_turn — 用 last_message 的图片构造媒体 parts
let media = build_media_parts(&session.last_message);   // image_urls/base64 → ContentPart
if media.is_empty() {
    session.add_user(user_content);
} else {
    session.add_user_with_media(user_content, media);
}
// persist_hook 随后把这条含图片的消息写入 history.jsonl（不变）
```

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

辅助模型默认复用 `[routing.chat]` 链（§4.2 优先级 2），**无需任何配置即可工作**。
仅当用户有多个视觉模型、想指定其中某个作辅助（控制成本/延迟/质量）时才需要：

```toml
# 可选：为图片理解指定专用辅助模型（不配则复用 [routing.chat] 链中的视觉模型）
[routing.image_aux]
models = ["gpt-4o-mini"]

# Phase 2/3：
# [routing.audio_aux]
# models = ["gpt-4o-audio"]
```

**实现注意**：现有 `RoutingConfig.get()`（`routing.rs:52`）只接受 `Capability` 枚举，键硬编码为
7 个 capability，**读不出 `image_aux` 这类约定键**。底层虽是 `HashMap<String, RouteEntry>`，
但需为 `RoutingConfig` 新增一个按字符串键查询的方法（如 `get_by_key("image_aux")`），
`aux_model_for(modality)` 借此读取首个 model。这是相对原 RFC「复用即可」说法的一处实现修正。

> 鉴于多视觉模型场景在当前并不常见，§4.6 整体可作为 **Phase 1.5** 推迟：v1 先只做优先级 2
> （复用 chat 链）+ 占位符降级，零新增配置；待真有「指定辅助模型」需求再加此键，符合 YAGNI。

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
add_user_with_media(text, [ImageUrl])   ← 图片进 history（v3）
persist_hook → history.jsonl 含 ImageUrl
    ↓
messages = clone(history)   ← 已带 ImageUrl parts
    ↓
model_supports_images = true
    ↓
保持 parts 原样（无需快照拼接）
    ↓
render → API（带图片）
```

### 5.2 主模型不支持图片（新增流程）

```
用户发图 + 文本
    ↓
add_user_with_media(text, [ImageUrl])   ← 图片进 history（v3）
persist_hook → history.jsonl 含 ImageUrl
    ↓
messages = clone(history)   ← 已带 ImageUrl parts（当轮+历史）
    ↓
model_supports_images = false
    ↓
last_user_idx = 末条 user 消息下标
adapt_history_media(skip=last_user_idx):  更早消息里的 ImageUrl
  ├─ 缓存命中 → "[图片描述]: ..."（复用，多轮追问可答）
  └─ 缓存未命中 → "[image]"
adapt_last_turn_media(messages[last_user_idx]):  当轮新图片（已在该消息 parts 内）
  ├─ 找到辅助视觉模型（[routing.image_aux] 覆盖 → [routing.chat] 链中的视觉模型）
  ├─ 查缓存 → miss 则流式 Chat 调用 from_stream() → 写缓存
  ├─ 多图并行 join_all
  └─ 用文本 part 替换该消息的图片 parts: "[图片描述]: 一张包含...的图片"
    ↓
render → API（纯文本）
    ↓
持久化：history.jsonl 中 ImageUrl parts 始终保留（clone 层变换不写回）
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
| 不强制新增配置块 | 默认复用 `[routing.chat]` 链即可工作；`[routing.image_aux]` 仅为可选覆盖 |
| 不做全局自动发现 | 候选集限定在用户显式声明的路由内，符合 MyClaw 显式路由哲学，行为可预测 |
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
7. **持久化验证**：clone 层的媒体变换不写回——`load_messages` 后图片 parts 与持久化前一致（大图经 `ImageRef` 外置后 hydrate 回等价 `ImageB64`，见 §11.6 blob 往返）
8. **Compaction 回归**：验证 `strip_images()` 行为不变
9. **多图并行**：多张图片 `join_all` 并行翻译，结果顺序与注入文案编号一致
10. **辅助模型失败**：单图失败 → `[translation failed]`，不影响其他图片（graceful degradation）
11. **显式 aux 覆盖**：配置 `[routing.image_aux]` 时验证优先于 `[routing.chat]` 链选中
12. **候选集边界**：注册但未列入 `[routing.chat]` / `image_aux` 的视觉模型，验证**不会**被选为辅助模型

## 10. 变更清单

| 文件 | 变更类型 | 说明 |
|------|---------|------|
| `src/providers/capability.rs` | 新增方法 | 仅 `supports_input(modality)`（`supports_image_input()` 已存在） |
| `src/providers/provider_registry.rs` | 新增方法 | `find_chat_model_with_modality()`（可为 trait 默认方法） |
| `src/registry/mod.rs` | 新增实现 | `aux_model_for(modality)`（读 `[routing.*_aux]`）；如不用默认方法则实现 `find_chat_model_with_modality()` |
| `src/config/routing.rs` | 可选（Phase 1.5） | 新增 `get_by_key(&str)` 以读取约定键 `image_aux` 等（`get()` 仅认 Capability 键）；v1 可不做 |
| `src/agents/modality_adapter.rs` | **新增文件** | `ModalitySpec` / `DescriptionCache` / `translate_part`（流式）/ `adapt_history_media` / `adapt_pending_media` |
| `src/agents/runtime.rs` | 新增字段 | `description_cache: Arc<dyn DescriptionCache>` 单例 |
| `src/agents/session/types.rs` | **新增方法** | `add_user_with_media(text, Vec<ContentPart>)`——当轮图片进 history（§3.6） |
| `src/agents/session_context.rs` | 修改 | `process_turn` 由 `last_message` 构造媒体 parts，走 `add_user_with_media`；持久化随之带图 |
| `src/agents/agent.rs` | 修改 | `run()` 集成 `adapt_history_media(skip)` + `adapt_last_turn_media`；**删除**从 `last_message` 快照拼回图片的旧逻辑 |
| `src/agents/mod.rs` | 修改 | 声明 `mod modality_adapter` |
| `src/providers/capability_chat.rs` | 新增变体 | `ContentPart::ImageRef { hash, media_type, detail }`——blob 外置的磁盘表示（§11.8） |
| `src/storage/json_file.rs` | 修改 | `append_message` 前 `externalize`、`load_messages`/recovery 后 `hydrate`；`write_blob`/`read_blob`；`rotate_history`/`truncate_messages` 收尾 GC（§11.8） |
| `Cargo.toml` | 依赖 | `sha2`（指纹/blob 名）、`lru`（缓存）、`base64`（blob 编解码，多半已在用）；`futures-util` 已在用 |

## 11. 持久化体积与已知风险

把图片存进 history 带来正确性收益（§3.6），也引入需要明确处理的代价。逐项列出，含缓解策略。

### 11.1 持久化体积（主要代价）

- **b64 内联图**：一张图常达数 MB，base64 后更大。直接写进 `history.jsonl` 会显著膨胀历史文件，
  加载/反序列化变慢。
- **URL 图**：只是字符串链接，体积可忽略。
- **缓解**：
  1. **blob 外置（v1 主策略，详见 §11.8）**：大于阈值的 b64 字节落盘到 session 目录下的
     `blobs/`，`history.jsonl` 只存引用句柄（`ContentPart::ImageRef { hash }`），加载时取回。
     这从根本上消除 `history.jsonl` 的 b64 膨胀与解析变慢。
  2. **compaction 兜底**：`strip_images()`（§1.1）在压缩时把图片→`[image]`，长程历史的图片
     连同其 blob 一并被回收（§11.8.5 GC），与 blob 外置叠加，进一步收紧上限。
  3. **入站即转引用**：若渠道给的是可下载 URL，优先持久化 URL 而非 b64，体积更小。

### 11.2 URL 过期

- 持久化的 CDN/Telegram 文件 URL 可能过期；切回视觉模型若直接发过期 URL，调用会失败。
- **缓解**：
  - 缓存的**文本描述不过期**——非视觉模型路径不受影响。
  - 视觉模型路径：可在入站时把 URL 图**下载为 b64**再持久化（牺牲体积换不失效），
    或在发送前探测/刷新。两者皆为 Phase 1.5 优化，v1 先接受「过期 URL 发给视觉模型可能失败」
    并按现有错误路径处理。

### 11.3 与 compaction `strip_images` 的协作

- 压缩后历史里的图片变成 `[image]` 文本，**fingerprint 随之消失**——后续无法再对该图缓存复用描述。
- 这是可接受的：被压缩的是远端过期上下文，本就不期望精细追问。
- 注意**顺序**：adapt 层在 clone 上工作、strip 在 compaction 持久化路径工作，二者不冲突；
  但需确保 strip 后的占位符文本不会被 `part_matches` 误判为媒体（它是 `Text`，不会）。

### 11.4 Token 计账

- 注入的描述文本会进入主模型上下文，**计入 token 预算**。多图长描述可能挤占窗口。
- **缓解**：`translate_part` 已设 `max_tokens: 1024` 限制单图描述长度；后续可按
  `ModalitySpec` 调节详略（§8.3 描述质量分级）。context window 估算需把注入描述计入。

### 11.5 Prompt Injection

- 辅助模型对图片的描述是**模型生成的不可信文本**，被原样注入主模型上下文；图片中嵌入的文字
  （如「忽略以上指令」）可能借描述通道触达主模型。
- **缓解**：注入文本统一包裹明确定界（`[图片描述]: ...`），并可在系统提示中声明
  「图片描述为不可信外部内容」。与渠道入站的其他不可信内容同级处理，不额外降权。

### 11.6 测试策略

- 辅助翻译走真实 `ChatProvider::chat()` 流式接口，单测需 **mock provider**：返回固定
  `BoxStream<StreamEvent>`，验证 `from_stream()` 聚合、缓存写入、并行 `join_all` 顺序。
- 持久化测试：构造含图 `ChannelMessage` → `process_turn` → 断言 `history.jsonl` 那条 user 消息
  含图片 part（§9 测试 7 强化）。
- **blob 往返**：大 b64 图 `append_message` → 断言 `history.jsonl` 行内是 `ImageRef`、`blobs/{hash}.bin`
  已生成；再 `load_messages` → 断言 hydrate 回 `ImageB64` 且字节与原图一致（§11.8.3/4）。
- **内联阈值**：≤阈值的小图持久化后仍为内联 `ImageB64`，不产生 blob 文件。
- **blob 去重**：同图在会话内多次出现 → 断言 `blobs/` 只有一个文件（content-addressed 幂等）。
- **blob GC**：`strip_images` + `rotate_history` 后 → 断言被剥离图的孤儿 blob 已删除、仍被引用的保留（§11.8.5）。
- **blob 缺失容错**：删掉 blob 文件后 `load_messages` → 断言降级为 `[image unavailable]`，加载不失败。

### 11.7 UX 可见性

- 非视觉模型回答基于描述而非原图时，用户可能不知情。可选：在回复或日志中标注
  「（图片经辅助模型 X 转述）」，便于用户判断可信度。属可选增强，不阻塞 v1。

### 11.8 Blob 外置详细设计（v1 纳入）

解决 §11.1 的 b64 膨胀：把图片字节从 `history.jsonl` 移到旁路 blob 文件，行内只留哈希引用。
**v1 即随图片持久化（§3.6）一并落地**——二者本就是同一条写路径上的两步。

#### 11.8.1 核心判断：ImageRef 是「持久化边界表示」，不是内存表示

现有存储（`src/storage/json_file.rs`）布局已经很适配：

```
{workspace}/sessions/{session_id}/
  history.jsonl          # 每行一个 ChatMessage JSON，append-only；行号=message id
  meta.json
  archive/history.NNNN.jsonl   # compaction 时归档的旧段
```

`ContentPart` 是 `#[serde(tag = "type")]`，渲染层（`protocols/openai|anthropic/
chat_message_rendering.rs`）、`strip_images`、§4.3 的 adapt 层、§4.3.1 的 `fingerprint`
**全都直接消费 `ImageB64`**。若让 `ImageRef` 在内存中流通，这些点都得改——成本高、易漏。

**因此 `ImageRef` 只活在磁盘上**：内存里的 `session.history` 永远是 `ImageB64`（hydrated），
转换收敛在 `JsonFileBackend` 的**写/读两个方法**里。除 backend 外，全代码库看不到 `ImageRef`，
渲染/adapt/fingerprint/strip **零改动**。这正是 §11.1 目标（磁盘体积 + 解析速度）的精确解，
而非更激进的「运行时省内存」（后者由 compaction 已经兜底）。

```
内存 (session.history / clone messages):   只有 ImageB64 / ImageUrl
        ↑ load: ImageRef → 读 blob → ImageB64      ↓ persist: ImageB64 → 写 blob → ImageRef
磁盘 (history.jsonl):                       ImageB64(小图内联) / ImageRef(大图引用) / ImageUrl
```

#### 11.8.2 新增 ContentPart 变体

```rust
// src/providers/capability_chat.rs — 仅持久化边界出现，#[serde(tag="type")] 自动得到
// {"type":"image_ref","hash":"...","media_type":"image/png","detail":"auto"}
ImageRef {
    /// sha256(图片字节) 的十六进制；同时是 blob 文件名与缓存 fingerprint。
    hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    media_type: Option<String>,
    detail: ImageDetail,
},
```

> `hash` 复用为 §4.3.1 fingerprint 的 key（对 b64 内容取 sha256），保证 blob 文件名、缓存 key、
> 内存 `ImageB64` 三者指纹一致——外置不破坏缓存命中。

#### 11.8.3 落盘布局与写路径

```
{session_id}/blobs/{sha256}.bin     # 原始图片字节（解码后的二进制，非 base64）
```

`JsonFileBackend::append_message` 在序列化前做一次 `externalize`：

```rust
const INLINE_MAX: usize = 8 * 1024;   // 阈值：≤8KB 的小图仍内联，省得为缩略图建 blob

fn externalize(&self, session_id: &str, msg: &ChatMessage) -> ChatMessage {
    let mut out = msg.clone();
    for part in out.parts.iter_mut() {
        if let ContentPart::ImageB64 { b64_json, media_type, detail } = part {
            if b64_json.len() <= INLINE_MAX { continue; }       // 小图内联
            let bytes = base64::decode(b64_json).unwrap_or_default();
            let hash = format!("{:x}", Sha256::digest(&bytes));
            self.write_blob(session_id, &hash, &bytes);          // 幂等：存在则跳过（天然去重）
            *part = ContentPart::ImageRef { hash, media_type: media_type.take(), detail: *detail };
        }
    }
    out
}
```

`write_blob` 幂等：文件已存在即不重写——**同图在会话内只存一份**（按内容 sha256 去重）。
作用域选 **per-session**：清理免费（`delete_session` 已 `remove_dir_all` 整个目录）；
跨会话全局去重留作后续（需引用计数 GC，复杂度不值当）。

#### 11.8.4 读路径（hydrate）

`load_messages`（及 recovery、archive 读取）读出每行后做一次 `hydrate`：

```rust
fn hydrate(&self, session_id: &str, msg: &mut ChatMessage) {
    for part in msg.parts.iter_mut() {
        if let ContentPart::ImageRef { hash, media_type, detail } = part {
            match self.read_blob(session_id, hash) {                 // {id}/blobs/{hash}.bin
                Ok(bytes) => *part = ContentPart::ImageB64 {
                    b64_json: base64::encode(&bytes),
                    media_type: media_type.take(),
                    detail: *detail,
                },
                Err(_) => *part = ContentPart::Text {                // blob 缺失：降级占位
                    text: "[image unavailable]".into(),
                },
            }
        }
    }
}
```

> blob 缺失（手动删档/损坏）降级为占位文本，不让整条历史加载失败——与 §11.2 URL 过期同等容错。

#### 11.8.5 Blob GC

blob 在两种情况下变成孤儿，需回收：

1. **Compaction/rotation**：`strip_images` 把 `ImageB64`→`[image]`，旧段 `rotate_history` 归档后，
   被剥离的图不再被任何存活消息引用。
2. **truncate_messages**（turn 回滚）：截断掉的消息引用的 blob 可能孤立。

**策略**：在 `rotate_history` / `truncate_messages` 收尾处做一次**标记-清扫**——
扫描存活消息（含归档段，若归档也外置）收集所有 `ImageRef.hash` 集合，
删除 `blobs/` 中不在集合内的文件。简单、无需引用计数；blob 数量级小，全扫成本可忽略。

> 归档段（`archive/history.NNNN.jsonl`）是否也外置：建议**是**（归档恰恰是大体积重灾区），
> 则 GC 的「存活集合」需并入归档段的引用。若归档不外置，则归档前需先 hydrate 回 b64（体积回潮）——
> 故推荐归档也走 ImageRef。

#### 11.8.6 涉及文件

blob 外置的文件改动已并入 §10 主变更清单（`capability_chat.rs` 新增 `ImageRef` 变体、
`json_file.rs` 的 externalize/hydrate/GC、`Cargo.toml` 的 `base64`），作为 v1 范围的一部分。

> 注意：`strip_images`、渲染层、§4.3 adapt 层、`fingerprint`、`InMemoryBackend` **均不改**——
> 这是「持久化边界表示」选型的核心收益。`InMemoryBackend` 无磁盘、不外置，天然正确。

## 12. 实现路线与任务清单

> **执行约定（重要）**：每完成一个任务，必须**同步更新 `docs/architecture.md`** 对应模块章节
> （新增类型/方法/字段、改动的数据流），并勾选下方复选框。架构文档与代码同提交，不滞后。
> 见 §12.4 的「任务 → architecture.md 章节」映射。

### 12.1 目标（验收标准）

| # | 目标 | 衡量标准 |
|---|------|---------|
| G1 | 非视觉主模型也能"看懂"图片 | DeepSeek V3 收图 → 辅助模型转述 → 能回答图片相关问题 |
| G2 | 多轮追问历史图片仍可答 | Turn N 发图、N+1 追问 → 复用缓存描述，非 `[image]` |
| G3 | 切回视觉模型能用原图 | 历史图持久化在 history，视觉模型轮原生使用原图 |
| G4 | 持久化图片不撑爆 `history.jsonl` | 大 b64 外置为 blob，行内只存 `ImageRef` |
| G5 | 零侵入现有渲染/压缩链路 | 渲染层、`strip_images`、`fingerprint`、`InMemoryBackend` 不改 |
| G6 | 模态可扩展 | audio/video 仅加 `ModalitySpec`，核心逻辑不重复 |

### 12.2 分阶段任务

#### Stage 0 — 能力查询基建（无依赖，可先行）
- [x] **T0.1** `capability.rs`：加 `supports_input(modality)`（`supports_image_input` 已存在）
- [x] **T0.2** `provider_registry.rs`：加 trait 方法 `find_chat_model_with_modality()`（可默认实现）
- [x] **T0.3** `registry/mod.rs`：实现 `aux_model_for(modality)`（读 `[routing.*_aux]`，无配置返回 `None`）
- [ ] **T0.4**（可选 / Phase 1.5）`config/routing.rs`：`get_by_key(&str)` 读约定键 `image_aux`

#### Stage 1 — 持久化 + blob 外置（核心数据流，G3/G4）
- [x] **T1.1** `capability_chat.rs`：新增 `ContentPart::ImageRef { hash, media_type, detail }`（仅磁盘表示）
- [x] **T1.2** `json_file.rs`：`write_blob` / `read_blob`（`sessions/{id}/blobs/{sha256}.bin`）
- [x] **T1.3** `json_file.rs`：`append_message` 前 `externalize`（大 `ImageB64`→`ImageRef`+写 blob，阈值 8KB）
- [x] **T1.4** `json_file.rs`：`load_messages`/recovery 后 `hydrate`（`ImageRef`→`ImageB64`；缺失→`[image unavailable]`）
- [x] **T1.5** `json_file.rs`：`rotate_history` / `truncate_messages` 收尾 blob GC（标记-清扫）
- [x] **T1.6** `session/types.rs`：`add_user_with_media(text, Vec<ContentPart>)`
- [x] **T1.7** `session_context.rs`：`process_turn` 由 `last_message` 构造媒体 parts → 走 `add_user_with_media`

#### Stage 2 — 模态适配模块（G1/G2/G6）
- [x] **T2.1** 新建 `modality_adapter.rs`：`ModalitySpec` + `IMAGE_SPEC`、`part_matches`、`fingerprint`
- [x] **T2.2** `DescriptionCache` trait + LRU 实现
- [x] **T2.3** `runtime.rs`：挂 `description_cache: Arc<dyn DescriptionCache>` 单例
- [x] **T2.4** `translate_part`：流式 `chat()` → `ChatResponse::from_stream()`，查/写缓存
- [x] **T2.5** `adapt_history_media(messages, spec, cache, skip_idx)`：缓存复用/占位符，不调辅助模型
- [x] **T2.6** `adapt_last_turn_media(&mut msg, spec, aux, cache)`：并行 `join_all` 翻译末条 user 图，替换 parts

#### Stage 3 — agent.rs 集成 + 收尾（G5）
- [x] **T3.1** `agent.rs`：`model_supports_images == false` 分支接 `adapt_history_media(skip)` + `adapt_last_turn_media`
- [x] **T3.2** `agent.rs`：**删除**从 `last_message` 快照拼回图片的旧逻辑（history 已带图）
- [x] **T3.3** `agents/mod.rs`：声明 `mod modality_adapter`
- [x] **T3.4** `Cargo.toml`：`sha2` / `lru` / `base64` 依赖确认

#### Stage 4 — 测试（覆盖 §9 + §11.6）
- [ ] **T4.1** 回归：视觉模型流程不受影响
- [ ] **T4.2** 非视觉 + 有/无辅助模型：转述 / 占位符
- [x] **T4.3** 多轮缓存命中、跨会话去重（fingerprint 仅调一次）
- [x] **T4.4** blob：往返一致、内联阈值、去重单文件、GC 回收孤儿、缺失降级
- [ ] **T4.5** 多图并行顺序、辅助失败 graceful、候选集边界（未入路由不被选）

> **测试覆盖现状**：模块级单测已落地——Stage 1 的 5 个 blob 测试（往返/阈值/去重/GC/缺失降级，
> 对应 T4.4 ✅）、Stage 2 的 5 个 adapter 测试（流式聚合+写缓存、缓存命中跳过 provider 调用、
> `join_all` 顺序与编号合并、无 aux 占位降级、`adapt_history_media` 的缓存-vs-占位+`skip_idx`，
> 对应 T4.3 ✅ 及 T4.2/T4.5 的适配层逻辑）。**延后**：T4.1/T4.2/T4.5 的 `agent.rs::run()`
> 端到端用例（视觉模型不重复附图、非视觉整链转述、候选集边界）需要一个可复用的 mock
> `ProviderRegistry` + `AgentRuntime` 测试骨架（当前 `NullRegistry`/`test_runtime` 是 orchestrator
> 私有 `#[cfg(test)]`，未导出）。Stage 3 的 `adapt_media_for_model` 是薄胶水层、其底层两函数已被
> 上述单测覆盖，且图片重复已通过删除旧快照路径从结构上消除，故端到端用例作为紧后续补齐。
> T0.4（`[routing.*_aux]` 显式覆盖键）仍按 §4.6 推迟，`aux_model_for` 暂回退到 `[routing.chat]` 链。

### 12.3 依赖与关键路径

```
Stage 0 ─┐
         ├─→ Stage 2 ─┐
Stage 1 ─┴────────────┴─→ Stage 3 ─→ Stage 4
```

- **可并行**：Stage 0 / 1 / 2 相互独立，可同时开工
- **关键路径**：Stage 1（数据流最深、改动最广）→ Stage 3 集成
- **最小可演示里程碑**：Stage 0 + 2 + 3（不含 blob）即跑通 G1/G2；Stage 1 补 G3/G4

### 12.4 任务 → `architecture.md` 同步映射

每个任务完成时，更新 `docs/architecture.md` 中对应模块章节：

| 任务 | architecture.md 章节 | 更新内容 |
|------|---------------------|---------|
| T0.1 | `## providers/` → `#### providers/capability.rs` | 新增 `supports_input` 方法签名 |
| T0.2 / T0.3 | `## providers/` / `## registry/` | `find_chat_model_with_modality` / `aux_model_for` |
| T0.4 | `## config/` | `RoutingConfig::get_by_key` |
| T1.1 | `## providers/` → `#### providers/capability_chat.rs` | `ContentPart::ImageRef` 变体 |
| T1.2–T1.5 | `## storage/` → `#### storage/json_file.rs` | blob 布局、externalize/hydrate/GC、`blobs/` 目录 |
| T1.6 / T1.7 | `## agents/` → `agents/session`、`agent.rs` | `add_user_with_media`、`process_turn` 持久化带图 |
| T2.1–T2.6 | `## agents/`（新增 `agents/modality_adapter.rs` 小节） | 整个新模块的类型与函数 |
| T2.3 | `## agents/` → `runtime.rs` / `AgentRuntime` | 新增 `description_cache` 字段 |
| T3.1–T3.3 | `## agents/` → `#### agents/agent.rs`、`mod.rs` | 集成点、删除快照逻辑、模块声明 |
| 全部 | `## 模块依赖关系图` | 若新增跨模块依赖（如 `agents` → `modality_adapter`）需更新依赖图 |

> **DoD（任务完成定义）**：代码改动 + 单测通过 + `architecture.md` 对应章节已更新 + 本节复选框勾选，
> 四者齐全方算完成。架构文档滞后视为任务未完成。
