//! LLM stream helpers — timeout constants and chunk-reading utilities.
//!
//! RFC v2 §六.A: the stream timeouts are deliberately hardcoded constants,
//! NOT config-exposed. Per-agent overrides were intentionally removed —
//! these bound provider/network stalls and are not an operator knob. Tune
//! them here if upstream model latencies change.

use anyhow::{Result, anyhow};
use futures::StreamExt;
use std::time::Duration;
use tokio::time::timeout;

use crate::providers::{BoxStream, StreamEvent};

/// Time to wait for the first stream chunk before giving up.
///
/// 300 s covers slow models (DeepSeek reasoning, GPT-5 thinking) on very
/// large prompts (300k+ tokens) behind proxies (CPA) where upstream prefill
/// + thinking time-to-first-token regularly exceeds the old 60 s bound.
///
/// The pre-stream `send()` is bounded separately by [`REQUEST_SEND_TIMEOUT`],
/// so this timeout only ever applies once the provider has accepted the
/// request and opened a stream.
pub const STREAM_FIRST_CHUNK_TIMEOUT: Duration = Duration::from_secs(300);

/// Time to wait between chunks once streaming has started.
///
/// Lower than first-chunk because mid-stream stalls indicate a real broken
/// connection. 120 s is enough to ride out brief proxy hiccups.
pub const STREAM_CHUNK_INTERVAL_TIMEOUT: Duration = Duration::from_secs(120);

/// Hard ceiling on one full stream-read, from first chunk to terminal event.
///
/// Bounds the worst case of many small-but-alive chunks trickling forever
/// (each individually under the interval timeout) and ensures the whole turn
/// cannot run past a bounded wall-clock budget. 500 s > first-chunk (300 s) +
/// headroom so a single slow chunk never trips it spuriously.
pub const STREAM_TOTAL_TIMEOUT: Duration = Duration::from_secs(500);

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
        Err(_) => Err(anyhow!("stream timed out after {}s", timeout_dur.as_secs())),
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
            // A truncated provider stream surfaces as Error/HttpError; don't
            // silently return a partial summary as if it were complete.
            Some(StreamEvent::Error(e)) => return Err(anyhow!("stream error: {e}")),
            Some(StreamEvent::HttpError { status, message }) => {
                return Err(anyhow!("stream HTTP {status}: {message}"));
            }
            Some(_) => {}
            None => break,
        }
    }
    Ok(text)
}
