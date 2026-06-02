//! LLM stream helpers — timeout constants and chunk-reading utilities.
//!
//! RFC v2 §六.A: the stream-first-chunk timeout becomes a per-call hardcoded
//! constant; per-agent override is removed.

use anyhow::{anyhow, Result};
use futures::StreamExt;
use std::time::Duration;
use tokio::time::timeout;

use crate::providers::{BoxStream, StreamEvent};

/// Time to wait for the first stream chunk before giving up.
///
/// 600 s covers slow models (DeepSeek reasoning, GPT-5 thinking) on long
/// prompts. Triggered on cold inference or a stalled upstream.
pub const STREAM_FIRST_CHUNK_TIMEOUT: Duration = Duration::from_secs(600);

/// Time to wait between chunks once streaming has started.
///
/// Lower than first-chunk because mid-stream stalls indicate a real broken
/// connection. 120 s is enough to ride out brief proxy hiccups.
pub const STREAM_CHUNK_INTERVAL_TIMEOUT: Duration = Duration::from_secs(120);

/// Read the next event from a stream with a timeout.
///
/// Returns `Ok(Some(event))` on success, `Ok(None)` on stream end,
/// `Err` on timeout or upstream error.
pub async fn read_next(
    stream: &mut BoxStream<StreamEvent>,
    timeout_dur: Duration,
) -> Result<Option<StreamEvent>> {
    match timeout(timeout_dur, stream.next()).await {
        Ok(Some(ev)) => Ok(Some(ev)),
        Ok(None) => Ok(None),
        Err(_) => Err(anyhow!(
            "stream timed out after {}s",
            timeout_dur.as_secs()
        )),
    }
}

/// Read a complete stream and collect text deltas into a buffer.
///
/// Used by non-streaming callers (e.g., compaction summarizer) that just want
/// the final text without per-chunk handling.
pub async fn read_to_string(stream: BoxStream<StreamEvent>) -> Result<String> {
    let mut s = stream;
    let mut text = String::new();
    loop {
        match read_next(&mut s, STREAM_CHUNK_INTERVAL_TIMEOUT).await? {
            Some(StreamEvent::Delta { text: delta }) => text.push_str(&delta),
            Some(StreamEvent::Done { .. }) => break,
            Some(_) => {}
            None => break,
        }
    }
    Ok(text)
}
