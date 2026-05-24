//! `run_turn_core` — preamble/postamble wrapper for a single conversation turn.
//!
//! Extracted from `run.rs` to keep that file focused on the public entry points
//! (`run`, `run_streamed`) and stream-collection helpers.

use super::AgentLoop;
use super::types::{StreamMode, estimate_message_tokens};
use crate::providers::ChatMessage;

/// Shared implementation for `run()` and `run_streamed()`.
///
/// Preamble (reset, diff, persist user message) → chat_loop → postamble (persist assistant).
pub(super) async fn run_turn_core(
    agent: &mut AgentLoop,
    user_message: &str,
    image_urls: Option<Vec<String>>,
    image_base64: Option<Vec<String>>,
    stream_mode: StreamMode,
) -> anyhow::Result<String> {
    use super::super::TurnEvent;

    // Clone event_tx early so we can send terminal error events even after
    // stream_mode is moved into chat_loop (where it is consumed).
    let error_event_tx = if let StreamMode::Streamed { ref event_tx, .. } = stream_mode {
        Some(event_tx.clone())
    } else {
        None
    };

    // ── Breakpoint recovery: auto-resume interrupted turn ─────────────
    // If the session ends with assistant tool_calls that have no matching
    // tool results (process was killed mid-turn), re-execute the missing
    // tools and let chat_loop continue from there.
    let _recovery_text = agent.recover_incomplete_turn(&stream_mode).await?;

    // Reset loop breaker for new turn.
    agent.loop_breaker.reset();

    // Initialize token tracker for fresh session / recovery.
    if agent.context.is_fresh() {
        if let Some(stored) = agent.session.last_total_tokens {
            agent.context.init_from_stored(stored);
        } else {
            agent.context.init_from_history(
                agent.request_builder.system_prompt(),
                &agent.session.history,
            );
        }
    }

    // 1+2. Hot-reload check + attachment diffs (before adding the user message).
    agent.request_builder.refresh(&agent.session);
    tracing::debug!(
        pending_keys = ?agent.request_builder.pending_keys(),
        "run: diff complete"
    );

    // 3. Merge attachment text into the user message.
    let combined_user = agent.request_builder.merge_attachments(user_message);
    agent.request_builder.clear_pending();

    // 4. Add combined user message to history and persist.
    let user_msg = ChatMessage::user_text(combined_user.clone());
    agent.context.record_pending(estimate_message_tokens(&user_msg));
    // ★ Record snapshot length BEFORE adding user message, so rollback can
    //   undo everything added during this turn (user + assistant/tool_calls/tool_results).
    let turn_snapshot_len = agent.session.history.len();
    agent.session.add_user(combined_user.clone());

    if let Some(ref hook) = agent.persist_hook {
        if let Some(msg) = agent.session.history.last() {
            if let Some(id) = hook.persist_message(&agent.session.id, msg) {
                if let Some(last_id) = agent.session.message_ids.last_mut() {
                    *last_id = id;
                }
            }
        }
    }

    agent.request_builder.set_images(image_urls, image_base64);

    // 5. Build the full message list for this turn (pure: no side effects).
    let messages = agent.request_builder.build(&agent.session);

    // 3. Run the chat loop (handles tool calls iteratively).
    let text = match super::chat_loop::chat_loop(agent, messages, stream_mode).await {
        Ok(text) => text,
        Err(e) => {
            // Roll back turn for ALL errors so the user can retry cleanly.
            tracing::warn!(
                turn_snapshot_len,
                current_len = agent.session.history.len(),
                err = %e,
                "chat_loop failed, rolling back turn"
            );

            // Roll back in-memory history to pre-turn state.
            agent.session.rollback_to(turn_snapshot_len);

            // Roll back persisted history.
            if let Some(ref hook) = agent.persist_hook {
                hook.truncate_messages(&agent.session.id, turn_snapshot_len);
            }

            // Notify streaming client of the error so it is not left hanging.
            // stream_mode was moved into chat_loop, so we use the pre-cloned sender.
            if let Some(ref tx) = error_event_tx {
                let _ = tx.send(TurnEvent::Error { message: e.to_string() }).await;
            }

            // Check if this is a LoopBreak error — re-raise with specific type
            // so the orchestrator can show a tailored retry prompt.
            if let Some(crate::agents::error::AgentError::LoopBreak { reason }) =
                e.downcast_ref::<crate::agents::error::AgentError>()
            {
                return Err(crate::agents::error::AgentError::LoopBreak {
                    reason: reason.clone(),
                }.into());
            }

            // Propagate as-is (already rolled back).
            return Err(e);
        }
    };

    // 5. Handle empty response: rollback turn and return error.
    //    chat_loop retries internally (stream timeout × 3, empty response × 3).
    //    If it still returns empty, the turn is irrecoverable.
    //
    //    BUT: if the empty response is due to a checkpoint exit (SIGUSR1),
    //    skip persistence and return cleanly — let the session stay at the
    //    last tool_result so a new process can resume from the breakpoint.
    if text.is_empty() && crate::is_shutting_down() {
        tracing::info!("checkpoint exit with empty response, skipping persistence");
        return Ok(text);
    }

    if text.is_empty() {
        tracing::warn!(
            turn_snapshot_len,
            current_len = agent.session.history.len(),
            "empty response after retries, rolling back turn"
        );

        // Roll back in-memory history to pre-turn state.
        agent.session.rollback_to(turn_snapshot_len);

        // Roll back persisted history.
        if let Some(ref hook) = agent.persist_hook {
            hook.truncate_messages(&agent.session.id, turn_snapshot_len);
        }

        // Notify streaming client so it is not left hanging with isGenerating=true.
        if let Some(ref tx) = error_event_tx {
            let _ = tx.send(TurnEvent::Error {
                message: "模型未返回有效回复，请稍后重试".to_string(),
            }).await;
        }

        return Err(crate::agents::error::AgentError::EmptyResponse {
            user_message: combined_user,
        }.into());
    }

    // 5. Persist assistant response.
    agent.session.add_assistant(text.clone());

    // Persist assistant message via hook; capture the assigned DB id.
    if let Some(ref hook) = agent.persist_hook {
        if let Some(msg) = agent.session.history.last() {
            if let Some(id) = hook.persist_message(&agent.session.id, msg) {
                if let Some(last_id) = agent.session.message_ids.last_mut() {
                    *last_id = id;
                }
            }
        }
    }

    Ok(text)
}
