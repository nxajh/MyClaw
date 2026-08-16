//! End-to-end verification of the multimodal path, exercising the REAL wiring
//! offline (only `ChatProvider::chat` — the network boundary — is mocked):
//!
//!   real `Registry::register_chat` (wraps each provider in
//!   `MediaLoweringProvider`) → real fallback chain → real `Agent::run` →
//!   real `ViewImageTool`.
//!
//! Scenario: a text-only primary + a vision model in the chain, and a user
//! message carrying a real image. We assert the full loop:
//!   1. the text-only model receives the image lowered to a `[图片: path]` marker
//!      (provider-layer, per-model lowering — NOT pre-rendered by the agent);
//!   2. the model's `view_image` call is dispatched (proving advertise+execute);
//!   3. the tool sends the REAL image to the vision model;
//!   4. the vision model's answer flows back into the conversation.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;

use crate::agents::session::Session;
use crate::agents::turn::TurnContext;
use crate::agents::{Agent, AgentRuntime};
use crate::config::agent::{PermissionMode, RunMode};
use crate::config::sub_agent::SubAgentConfig;
use crate::providers::ProviderRegistry;
use crate::providers::capability::{ChatModelConfig, Modality};
use crate::providers::capability_chat::{
    BoxStream, ChatMessage, ChatProvider, ChatRequest, ContentPart, StopReason, StreamEvent,
};
use crate::registry::Registry;

/// A `ChatProvider` that records the messages it receives on each call and
/// replays a scripted sequence of `StreamEvent`s per call (FIFO).
struct ScriptedProvider {
    seen: Arc<Mutex<Vec<Vec<ChatMessage>>>>,
    scripts: Arc<Mutex<VecDeque<Vec<StreamEvent>>>>,
}

impl ScriptedProvider {
    fn new(scripts: Vec<Vec<StreamEvent>>) -> (Self, Arc<Mutex<Vec<Vec<ChatMessage>>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let p = Self {
            seen: Arc::clone(&seen),
            scripts: Arc::new(Mutex::new(scripts.into_iter().collect())),
        };
        (p, seen)
    }
}

impl ChatProvider for ScriptedProvider {
    fn chat(&self, req: ChatRequest<'_>) -> anyhow::Result<BoxStream<StreamEvent>> {
        self.seen.lock().unwrap().push(req.messages.to_vec());
        let events = self.scripts.lock().unwrap().pop_front().unwrap_or_else(|| {
            vec![StreamEvent::Done {
                reason: StopReason::EndTurn,
            }]
        });
        Ok(futures_util::stream::iter(events).boxed())
    }
}

fn text_cfg() -> ChatModelConfig {
    ChatModelConfig {
        input: vec![Modality::Text],
        output: vec![Modality::Text],
        context_window: Some(100_000),
        max_output_tokens: None,
        pricing: None,
        reasoning: false,
    }
}
fn vision_cfg() -> ChatModelConfig {
    ChatModelConfig {
        input: vec![Modality::Text, Modality::Image],
        output: vec![Modality::Text],
        context_window: Some(100_000),
        max_output_tokens: None,
        pricing: None,
        reasoning: false,
    }
}

fn empty_config() -> SubAgentConfig {
    SubAgentConfig {
        name: "test".into(),
        system_prompt: String::new(),
        tools: Default::default(),
        skills: Default::default(),
        mcp: Default::default(),
        max_tool_calls: None,
        description: None,
        model: None,
        isolation: Default::default(),
        timeout: None,
        max_timeout: None,
    }
}

fn runtime_with(providers: Arc<dyn ProviderRegistry>) -> AgentRuntime {
    use parking_lot::RwLock;
    let mut tools = crate::agents::ToolRegistry::new();
    // Register media retrieval tools — same as daemon.rs::build_tools.
    for builtin in crate::tools::builtin_tools(None) {
        tools.register(builtin);
    }
    tools.register(Arc::new(crate::tools::ViewImageTool::new(Arc::clone(
        &providers,
    ))));
    tools.register(Arc::new(crate::tools::HearAudioTool::new(Arc::clone(
        &providers,
    ))));
    tools.register(Arc::new(crate::tools::ViewVideoTool::new(Arc::clone(
        &providers,
    ))));
    let tools = Arc::new(tools);
    let skills = Arc::new(RwLock::new(crate::agents::SkillManager::new()));
    let agents = Arc::new(crate::agents::AgentRegistry::default());
    let resources = crate::agents::resource_provider::ResourceProvider::new(
        Arc::clone(&skills),
        Arc::clone(&agents),
        Vec::new(),
        std::path::PathBuf::new(),
        std::path::PathBuf::new(),
        String::new(),
        0,
    );
    let context_engine = Arc::new(crate::agents::context_engine::ContextEngine::new(
        &crate::config::agent::ContextConfig::default(),
        Arc::clone(&providers),
        resources,
        Arc::clone(&tools),
    ));
    let tool_executor = Arc::new(crate::agents::tool_executor::ToolExecutor::new(30));
    let loop_breaker = Arc::new(crate::agents::LoopBreaker::new(
        crate::agents::LoopBreakerConfig::default(),
    ));
    AgentRuntime::new(
        providers,
        tools,
        skills,
        agents,
        context_engine,
        tool_executor,
        loop_breaker,
    )
}

/// Registry routing (used by `get_chat_routing_models` / vision lookup).
fn registry_routing(models: &[&str]) -> crate::registry::routing::RoutingConfig {
    use crate::registry::routing::{RouteEntry, RoutingConfig, RoutingStrategy};
    RoutingConfig {
        chat: Some(RouteEntry {
            strategy: RoutingStrategy::Fallback,
            models: models.iter().map(|s| s.to_string()).collect(),
            providers: vec![],
        }),
        ..Default::default()
    }
}

/// Config routing (consumed by `maybe_wrap_chat_fallback`).
fn config_routing(models: &[&str]) -> crate::config::routing::RoutingConfig {
    use crate::config::routing::{RouteEntry, RoutingConfig, RoutingStrategy};
    use crate::providers::Capability;
    let mut rc = RoutingConfig::default();
    rc.insert(
        Capability::Chat,
        RouteEntry {
            strategy: RoutingStrategy::Fallback,
            models: models.iter().map(|s| s.to_string()).collect(),
            providers: vec![],
        },
    );
    rc
}

fn has_image(msgs: &[ChatMessage]) -> bool {
    msgs.iter().flat_map(|m| &m.parts).any(|p| match p {
        ContentPart::File {
            path, mime_type, ..
        } => {
            crate::providers::media::modality_from_mime(mime_type.as_deref(), path)
                == crate::providers::media::FileModality::Image
        }
        _ => false,
    })
}
fn joined_text(msgs: &[ChatMessage]) -> String {
    msgs.iter()
        .flat_map(|m| &m.parts)
        .filter_map(|p| match p {
            ContentPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn text_only_primary_reaches_image_via_view_image_end_to_end() {
    // Primary "text" is text-only; "vision" is image-capable. Both are in the
    // chat routing chain (text first), so the fallback serves with "text".
    let dir = tempfile::tempdir().unwrap();
    let image_path = dir.path().join("image.png");
    std::fs::write(&image_path, b"dummy").unwrap();

    let (text_provider, text_seen) = ScriptedProvider::new(vec![
        // Call 1: ask to look at image #1.
        vec![
            StreamEvent::ToolCallStart {
                id: "c1".into(),
                name: "view_image".into(),
                initial_arguments: String::new(),
            },
            StreamEvent::ToolCallEnd {
                id: "c1".into(),
                name: "view_image".into(),
                arguments: format!(
                    r#"{{"path":"{}","question":"图里是什么动物？"}}"#,
                    image_path.to_string_lossy()
                ),
            },
            StreamEvent::Done {
                reason: StopReason::ToolUse,
            },
        ],
        // Call 2: final answer after seeing the tool result.
        vec![
            StreamEvent::Delta {
                text: "这是一只猫。".into(),
            },
            StreamEvent::Done {
                reason: StopReason::EndTurn,
            },
        ],
    ]);
    let (vision_provider, vision_seen) = ScriptedProvider::new(vec![vec![
        StreamEvent::Delta {
            text: "A RED CAT".into(),
        },
        StreamEvent::Done {
            reason: StopReason::EndTurn,
        },
    ]]);

    let mut reg = Registry::new(Default::default(), registry_routing(&["text", "vision"]));
    reg.register_chat(
        Box::new(text_provider),
        "text".into(),
        text_cfg(),
        None,
        None,
    );
    reg.register_chat(
        Box::new(vision_provider),
        "vision".into(),
        vision_cfg(),
        None,
        None,
    );
    reg.maybe_wrap_chat_fallback(&config_routing(&["text", "vision"]));

    let runtime = runtime_with(Arc::new(reg));

    let mut session = Session::new("s-e2e".into());
    session.add_user_with_media(
        "图里是什么？".into(),
        vec![ContentPart::File {
            path: image_path.to_string_lossy().to_string(),
            mime_type: Some("image/png".into()),
            name: Some("image.png".into()),
            size_bytes: Some(5),
        }],
    );

    let agent = Agent::new(empty_config());
    let turn_ctx = TurnContext {
        system_prompt: "you are a test agent",
        model_id: None, // → fallback chain
        thinking: None,
        permission_mode: PermissionMode::Full,
        run_mode: RunMode::default(),
    };

    let result = agent
        .run(&mut session, turn_ctx, &runtime)
        .await
        .expect("turn ok");

    // (1) The text-only model's FIRST request had the image lowered to a marker.
    let text_seen = text_seen.lock().unwrap();
    assert!(
        text_seen.len() >= 2,
        "text model should be called twice (tool round-trip)"
    );
    assert!(
        !has_image(&text_seen[0]),
        "text-only model must NOT receive a native image part"
    );
    assert!(
        joined_text(&text_seen[0]).contains("[图片:"),
        "image must be lowered to a [图片: path] marker for the text-only model: {}",
        joined_text(&text_seen[0])
    );

    // (2)+(3) view_image was dispatched and sent the REAL image to the vision model.
    let vision_seen = vision_seen.lock().unwrap();
    assert_eq!(
        vision_seen.len(),
        1,
        "vision model should be called exactly once by the tool"
    );
    assert!(
        has_image(&vision_seen[0]),
        "view_image must forward the real image to the vision model"
    );
    assert!(
        joined_text(&vision_seen[0]).contains("图里是什么动物"),
        "the model's own question must reach the vision model"
    );

    // (4) The vision answer flowed back into the text model's SECOND request.
    assert!(
        joined_text(&text_seen[1]).contains("A RED CAT"),
        "vision description must return into the conversation: {}",
        joined_text(&text_seen[1])
    );

    assert_eq!(result.text, "这是一只猫。", "final assistant text");
}

fn audio_cfg() -> ChatModelConfig {
    ChatModelConfig {
        input: vec![Modality::Text, Modality::Audio],
        output: vec![Modality::Text],
        context_window: Some(100_000),
        max_output_tokens: None,
        pricing: None,
        reasoning: false,
    }
}

#[tokio::test]
async fn text_only_primary_reaches_audio_via_hear_audio_end_to_end() {
    // Primary "text" is text-only; "audio" is audio-capable. The user sends a
    // voice clip; the text model must reach it through `hear_audio`.
    let dir = tempfile::tempdir().unwrap();
    let audio_path = dir.path().join("voice.ogg");
    std::fs::write(&audio_path, b"dummy").unwrap();

    let (text_provider, text_seen) = ScriptedProvider::new(vec![
        vec![
            StreamEvent::ToolCallStart {
                id: "a1".into(),
                name: "hear_audio".into(),
                initial_arguments: String::new(),
            },
            StreamEvent::ToolCallEnd {
                id: "a1".into(),
                name: "hear_audio".into(),
                arguments: format!(
                    r#"{{"path":"{}","question":"用户说了什么？"}}"#,
                    audio_path.to_string_lossy()
                ),
            },
            StreamEvent::Done {
                reason: StopReason::ToolUse,
            },
        ],
        vec![
            StreamEvent::Delta {
                text: "用户在打招呼。".into(),
            },
            StreamEvent::Done {
                reason: StopReason::EndTurn,
            },
        ],
    ]);
    let (audio_provider, audio_seen) = ScriptedProvider::new(vec![vec![
        StreamEvent::Delta {
            text: "你好世界".into(),
        },
        StreamEvent::Done {
            reason: StopReason::EndTurn,
        },
    ]]);

    let mut reg = Registry::new(Default::default(), registry_routing(&["text", "audio"]));
    reg.register_chat(
        Box::new(text_provider),
        "text".into(),
        text_cfg(),
        None,
        None,
    );
    reg.register_chat(
        Box::new(audio_provider),
        "audio".into(),
        audio_cfg(),
        Some(crate::providers::ProviderId::new(
            crate::providers::provider_id::well_known::OPENAI,
        )),
        None,
    );
    reg.maybe_wrap_chat_fallback(&config_routing(&["text", "audio"]));

    let runtime = runtime_with(Arc::new(reg));

    let mut session = Session::new("s-e2e-audio".into());
    session.add_user_with_media(
        "听一下".into(),
        vec![ContentPart::File {
            path: audio_path.to_string_lossy().to_string(),
            mime_type: Some("audio/ogg".into()),
            name: Some("voice.ogg".into()),
            size_bytes: Some(5),
        }],
    );

    let agent = Agent::new(empty_config());
    let turn_ctx = TurnContext {
        system_prompt: "you are a test agent",
        model_id: None,
        thinking: None,
        permission_mode: PermissionMode::Full,
        run_mode: RunMode::default(),
    };

    let result = agent
        .run(&mut session, turn_ctx, &runtime)
        .await
        .expect("turn ok");

    let text_seen = text_seen.lock().unwrap();
    assert!(text_seen.len() >= 2, "text model called twice");
    assert!(
        joined_text(&text_seen[0]).contains("[语音:"),
        "audio must be lowered to a [语音: path] marker: {}",
        joined_text(&text_seen[0])
    );
    assert!(
        !text_seen[0]
            .iter()
            .flat_map(|m| &m.parts)
            .any(|p| matches!(p, ContentPart::File { .. })),
        "text-only model must NOT receive a native audio part"
    );

    let audio_seen = audio_seen.lock().unwrap();
    assert_eq!(audio_seen.len(), 1, "audio model called once by the tool");
    assert!(
        audio_seen[0]
            .iter()
            .flat_map(|m| &m.parts)
            .any(|p| matches!(p, ContentPart::File { .. })),
        "hear_audio must forward the audio file to the audio model"
    );

    assert!(
        joined_text(&text_seen[1]).contains("你好世界"),
        "transcription must return into the conversation: {}",
        joined_text(&text_seen[1])
    );
    assert_eq!(result.text, "用户在打招呼。");
}
