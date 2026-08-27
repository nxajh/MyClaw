//! 入站消息 debounce 合并窗（Telegram / WeChat 共享）。
//!
//! 两家渠道都有"同一发送者在同一会话中短时间内连发多条消息"的场景：
//! 各自维护一个 buffer，按 `sender|receiver` 为 key，窗口内到达的消息
//! 合并文本/文件，窗口过期后拼成一条 `ChannelInboundMessage` 派发；
//! 新消息到达时重置计时器。`window_ms == 0` 时禁用合并直接派发。
//! 这套 buffer/合并/计时器机制两家逐字同构，收编至此；渠道差异只有
//! 窗口时长与直发失败时是否告警（`dispatch_error_label`）。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::warn;

use crate::channels::message::{
    ChannelFile, ChannelInboundMessage, ChannelMessageContent, MessageReceiver, MessageSender,
};

/// debounce buffer 中的一条待合并消息。
struct DebounceBufferEntry {
    sender: MessageSender,
    receiver: MessageReceiver,
    texts: Vec<String>,
    files: Vec<ChannelFile>,
    first_ts: u64,
    timer: JoinHandle<()>,
}

/// 入站 debounce 合并器：`"sender|receiver"` → 待合并条目。
#[derive(Clone)]
pub struct InboundDebouncer {
    /// 合并窗口（毫秒）；0 = 禁用（直接派发）。
    window_ms: u64,
    /// 直发失败告警的渠道标签（如 "Telegram"）；`None` = 静默。
    dispatch_error_label: Option<&'static str>,
    buffer: Arc<Mutex<HashMap<String, DebounceBufferEntry>>>,
}

impl InboundDebouncer {
    pub fn new(window_ms: u64, dispatch_error_label: Option<&'static str>) -> Self {
        Self {
            window_ms,
            dispatch_error_label,
            buffer: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 当前合并窗口（毫秒）；0 = 禁用。
    pub fn window_ms(&self) -> u64 {
        self.window_ms
    }

    /// 该 sender/receiver 是否已有待合并条目（渠道用于"首条消息才启动
    /// typing"之类的判断，key 规则与 `push` 一致）。
    pub fn is_pending(&self, sender_id: &str, receiver_id: &str) -> bool {
        self.buffer
            .lock()
            .contains_key(&format!("{}|{}", sender_id, receiver_id))
    }

    /// 缓冲一条入站消息以进行 debounce 合并。
    ///
    /// 同一发送者在同一会话中的消息会被合并，窗口过期后作为单条
    /// `ChannelInboundMessage` 派发。debounce 禁用（`window_ms == 0`）时
    /// 经 `tx` 立即发送。
    pub async fn push(
        &self,
        mut msg: ChannelInboundMessage,
        tx: mpsc::Sender<ChannelInboundMessage>,
    ) {
        if self.window_ms == 0 {
            if let Err(e) = tx.send(msg).await {
                if let Some(label) = self.dispatch_error_label {
                    warn!("{label} dispatch error: {e}");
                }
            }
            return;
        }

        let key = format!("{}|{}", msg.sender.id, msg.receiver.id);
        let debounce_ms = self.window_ms;
        let buffer = self.buffer.clone();
        let sender_key = key.clone();

        // Create timer task (starts sleeping immediately).
        let handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(debounce_ms)).await;
            let entry = buffer.lock().remove(&sender_key);
            if let Some(entry) = entry {
                let channel_msg = ChannelInboundMessage {
                    id: format!("debounced_{}", entry.first_ts),
                    sender: entry.sender,
                    receiver: entry.receiver,
                    content: ChannelMessageContent {
                        text: entry.texts.join("\n"),
                        files: entry.files,
                        buttons: vec![],
                    },
                    timestamp: entry.first_ts,
                    interruption_scope_id: None,
                    silenced_override: None,
                    run_mode: Default::default(),
                };
                let _ = tx.send(channel_msg).await;
            }
        });

        // Lock the buffer and update/create entry.
        {
            let mut buf = self.buffer.lock();
            if let Some(entry) = buf.get_mut(&key) {
                // Merge into existing entry.
                if !msg.content.text.is_empty() {
                    entry.texts.push(msg.content.text);
                }
                if !msg.content.files.is_empty() {
                    entry.files.append(&mut msg.content.files);
                }
                // Cancel old timer, set new one.
                entry.timer.abort();
                entry.timer = handle;
            } else {
                // New entry.
                buf.insert(
                    key,
                    DebounceBufferEntry {
                        sender: msg.sender,
                        receiver: msg.receiver,
                        texts: if msg.content.text.is_empty() {
                            vec![]
                        } else {
                            vec![msg.content.text]
                        },
                        files: msg.content.files,
                        first_ts: msg.timestamp,
                        timer: handle,
                    },
                );
            }
        }
    }
}
