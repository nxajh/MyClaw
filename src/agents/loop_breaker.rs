//! LoopBreaker — detects repetitive tool-call patterns during the chat loop
//! and aborts the turn before the LLM spirals into an infinite loop.
//!
//! Three detection modes:
//!   1. **Exact repeat** — same tool name + same args called ≥ N times consecutively.
//!   2. **Ping-pong**      — two tools alternating back and forth ≥ R rounds.
//!   3. **No progress**    — same tool called ≥ P times with different args but
//!      the same result hash (suggesting the tool keeps returning the same output).

use std::collections::{HashSet, VecDeque};

// ── Public types ──────────────────────────────────────────────────────────────

/// Result of checking for loops.
#[derive(Debug, Clone, PartialEq)]
pub enum LoopBreak {
    /// No loop detected, continue.
    None,
    /// Loop detected. Contains the reason.
    Detected(LoopBreakReason),
}

#[derive(Debug, Clone, PartialEq)]
pub enum LoopBreakReason {
    /// Same tool + same args repeated too many times.
    ExactRepeat {
        tool: String,
        count: usize,
        threshold: usize,
    },
    /// Two tools ping-ponging.
    PingPong {
        tool_a: String,
        tool_b: String,
        rounds: usize,
    },
    /// Same tool, different args, same results.
    NoProgress {
        tool: String,
        count: usize,
    },
    /// Hard limit exceeded.
    MaxCalls {
        count: usize,
        limit: usize,
    },
}

/// Configuration for loop breaking.
#[derive(Debug, Clone)]
pub struct LoopBreakerConfig {
    /// Hard cap on total tool calls. 0 = unlimited (but still checks patterns).
    pub max_tool_calls: usize,
    /// Sliding window size for pattern detection.
    pub window_size: usize,
    /// Exact repeat threshold: same tool + same args N times → break.
    pub exact_repeat_threshold: usize,
    /// Ping-pong threshold: alternating rounds before breaking.
    pub ping_pong_rounds: usize,
    /// No-progress threshold: same tool + same result hash N consecutive times → break.
    pub no_progress_threshold: usize,
    /// Tools that are inherently exploratory (e.g. "shell") and need a higher threshold
    /// before NoProgress is triggered. These tools naturally produce similar results
    /// (empty grep, exit code 0) across different args without actually looping.
    pub relaxed_tools: Vec<String>,
}

impl Default for LoopBreakerConfig {
    fn default() -> Self {
        Self {
            max_tool_calls: 100,
            window_size: 20,
            exact_repeat_threshold: 3,
            ping_pong_rounds: 6,
            no_progress_threshold: 5,
            relaxed_tools: vec!["shell".to_string()],
        }
    }
}

// ── Internal types ────────────────────────────────────────────────────────────

/// A recorded tool invocation.
#[derive(Debug, Clone)]
struct ToolInvocation {
    tool_name: String,
    args_hash: u64,
    result_hash: u64,
}

// ── LoopBreaker ───────────────────────────────────────────────────────────────

/// Loop breaker — tracks tool call history and detects repetitive patterns.
pub struct LoopBreaker {
    config: LoopBreakerConfig,
    /// Total tool calls in this turn.
    total_calls: usize,
    /// Sliding window of recent invocations.
    window: VecDeque<ToolInvocation>,
}

impl LoopBreaker {
    pub fn new(config: LoopBreakerConfig) -> Self {
        let window_capacity = config.window_size;
        Self {
            config,
            total_calls: 0,
            window: VecDeque::with_capacity(window_capacity),
        }
    }

    /// Record a tool call + its result, then check for loops.
    /// Returns `LoopBreak::Detected` if a loop pattern is found.
    pub fn record_and_check(&mut self, tool_name: &str, args: &str, result: &str) -> LoopBreak {
        self.total_calls += 1;

        // Trim window if it exceeds max size.
        if self.window.len() >= self.config.window_size {
            self.window.pop_front();
        }

        let invocation = ToolInvocation {
            tool_name: tool_name.to_string(),
            args_hash: simple_hash(args),
            result_hash: simple_hash(result),
        };
        self.window.push_back(invocation);

        // 1. Hard limit check.
        if self.config.max_tool_calls > 0 && self.total_calls > self.config.max_tool_calls {
            return LoopBreak::Detected(LoopBreakReason::MaxCalls {
                count: self.total_calls,
                limit: self.config.max_tool_calls,
            });
        }

        // 2. Exact repeat check: count consecutive identical calls from the end.
        if let Some(reason) = self.check_exact_repeat() {
            return LoopBreak::Detected(reason);
        }

        // 3. Ping-pong check.
        if let Some(reason) = self.check_ping_pong() {
            return LoopBreak::Detected(reason);
        }

        // 4. No-progress check.
        if let Some(reason) = self.check_no_progress() {
            return LoopBreak::Detected(reason);
        }

        LoopBreak::None
    }

    /// Reset for a new turn.
    pub fn reset(&mut self) {
        self.total_calls = 0;
        self.window.clear();
    }

    /// Total calls recorded so far.
    pub fn total_calls(&self) -> usize {
        self.total_calls
    }

    // ── Pattern detectors ──────────────────────────────────────────────────

    fn check_exact_repeat(&self) -> Option<LoopBreakReason> {
        let window: Vec<_> = self.window.iter().rev().collect();
        if window.is_empty() {
            return None;
        }

        let first = window[0];
        let mut count = 1usize;
        for inv in window.iter().skip(1) {
            if inv.tool_name == first.tool_name && inv.args_hash == first.args_hash {
                // If the result differs, this is a polling pattern (same args,
                // evolving output) — not a true loop. Reset the count.
                if inv.result_hash != first.result_hash {
                    break;
                }
                count += 1;
            } else {
                break;
            }
        }

        if count >= self.config.exact_repeat_threshold {
            return Some(LoopBreakReason::ExactRepeat {
                tool: first.tool_name.clone(),
                count,
                threshold: self.config.exact_repeat_threshold,
            });
        }
        None
    }

    fn check_ping_pong(&self) -> Option<LoopBreakReason> {
        if self.window.len() < 4 {
            return None;
        }

        // Look at the last 2*N entries for alternating pattern.
        let n = self.config.ping_pong_rounds;
        let needed = n * 2;
        if self.window.len() < needed {
            return None;
        }

        let tail: Vec<_> = self.window.iter().rev().take(needed).collect();
        let tool_a = &tail[0].tool_name;
        let tool_b = &tail[1].tool_name;

        if tool_a == tool_b {
            return None; // Same tool, not ping-pong.
        }

        for (i, inv) in tail.iter().enumerate() {
            let expected = if i % 2 == 0 { tool_a } else { tool_b };
            if &inv.tool_name != expected {
                return None;
            }
        }

        // If all args for each tool are unique, the model is making progress
        // (different inputs each time), not stuck in a true loop.
        let args_a: Vec<u64> = tail.iter().step_by(2).map(|inv| inv.args_hash).collect();
        let args_b: Vec<u64> = tail.iter().skip(1).step_by(2).map(|inv| inv.args_hash).collect();
        let unique_a = args_a.iter().collect::<HashSet<_>>().len();
        let unique_b = args_b.iter().collect::<HashSet<_>>().len();
        if unique_a == args_a.len() && unique_b == args_b.len() {
            return None; // All args unique — genuine progress, not a loop.
        }

        Some(LoopBreakReason::PingPong {
            tool_a: tool_a.clone(),
            tool_b: tool_b.clone(),
            rounds: n,
        })
    }

    fn check_no_progress(&self) -> Option<LoopBreakReason> {
        // Count CONSECUTIVE calls from the end of the window where:
        //   - same tool name
        //   - same result hash (suggesting no progress)
        //   - different args_hash from the latest call (exact repeats handled separately)
        // Stop immediately when a different tool or different result is encountered.
        let window: Vec<_> = self.window.iter().rev().collect();
        if window.is_empty() {
            return None;
        }

        let target_tool = &window[0].tool_name;
        let target_result_hash = window[0].result_hash;
        let target_args_hash = window[0].args_hash;
        let mut count = 1usize;

        for inv in window.iter().skip(1) {
            // Must be strictly consecutive — break on any mismatch.
            if inv.tool_name != *target_tool || inv.result_hash != target_result_hash {
                break;
            }
            // Skip if same args as the latest (exact repeat is handled separately).
            if inv.args_hash == target_args_hash {
                continue;
            }
            count += 1;
        }

        // Use a higher threshold for relaxed tools.
        let threshold = if self.config.relaxed_tools.iter().any(|t| t == target_tool) {
            self.config.no_progress_threshold.saturating_mul(2).max(8)
        } else {
            self.config.no_progress_threshold
        };

        if count >= threshold {
            return Some(LoopBreakReason::NoProgress {
                tool: target_tool.clone(),
                count,
            });
        }
        None
    }
}

// ── Simple FNV-1a-style hash ──────────────────────────────────────────────────

/// Simple deterministic hash for detecting identical content.
/// Uses a basic FNV-1a approach over the full string (no need for cryptographic strength).
fn simple_hash(s: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in s.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn default_breaker() -> LoopBreaker {
        LoopBreaker::new(LoopBreakerConfig::default())
    }

    // ── Exact repeat ───────────────────────────────────────────────────────

    #[test]
    fn exact_repeat_triggers_after_threshold() {
        let mut lb = default_breaker();
        let tool = "read_file";
        let args = r#"{"path": "/tmp/x"}"#;
        let result = "contents of x";

        // First two calls: no loop.
        assert_eq!(lb.record_and_check(tool, args, result), LoopBreak::None);
        assert_eq!(lb.record_and_check(tool, args, result), LoopBreak::None);

        // Third call: triggers exact repeat (threshold = 3).
        let outcome = lb.record_and_check(tool, args, result);
        match outcome {
            LoopBreak::Detected(LoopBreakReason::ExactRepeat {
                tool: t,
                count,
                threshold,
            }) => {
                assert_eq!(t, "read_file");
                assert_eq!(count, 3);
                assert_eq!(threshold, 3);
            }
            other => panic!("expected ExactRepeat, got {:?}", other),
        }
    }

    #[test]
    fn exact_repeat_resets_on_different_call() {
        let mut lb = default_breaker();
        assert_eq!(
            lb.record_and_check("read_file", r#"{"p":"a"}"#, "ok"),
            LoopBreak::None
        );
        assert_eq!(
            lb.record_and_check("read_file", r#"{"p":"a"}"#, "ok"),
            LoopBreak::None
        );
        // Different args — should reset the consecutive count.
        assert_eq!(
            lb.record_and_check("read_file", r#"{"p":"b"}"#, "ok"),
            LoopBreak::None
        );
        // Only one consecutive call now; need two more of the same.
        assert_eq!(
            lb.record_and_check("read_file", r#"{"p":"b"}"#, "ok"),
            LoopBreak::None
        );
        let result = lb.record_and_check("read_file", r#"{"p":"b"}"#, "ok");
        match result {
            LoopBreak::Detected(LoopBreakReason::ExactRepeat { count, threshold, .. }) => {
                assert_eq!(count, 3);
                assert_eq!(threshold, 3);
            }
            other => panic!("expected ExactRepeat, got {:?}", other),
        }
    }

    #[test]
    fn exact_repeat_different_tool_resets() {
        let mut lb = default_breaker();
        assert_eq!(
            lb.record_and_check("read_file", "{}", "ok"),
            LoopBreak::None
        );
        assert_eq!(
            lb.record_and_check("read_file", "{}", "ok"),
            LoopBreak::None
        );
        // Different tool name — resets streak.
        assert_eq!(
            lb.record_and_check("write_file", "{}", "ok"),
            LoopBreak::None
        );
        assert_eq!(lb.total_calls(), 3);
    }

    #[test]
    fn exact_repeat_allows_polling_different_results() {
        // Same tool + same args, but results differ each time (polling pattern).
        // Should NOT trigger ExactRepeat.
        let mut lb = default_breaker();
        let args = r#"{"command": "gh run view 123"}"#;
        assert_eq!(
            lb.record_and_check("shell", args, "status: in_progress"),
            LoopBreak::None
        );
        assert_eq!(
            lb.record_and_check("shell", args, "status: in_progress, 2 jobs running"),
            LoopBreak::None
        );
        assert_eq!(
            lb.record_and_check("shell", args, "status: completed, conclusion: success"),
            LoopBreak::None
        );
        assert_eq!(
            lb.record_and_check("shell", args, "status: completed, conclusion: success"),
            LoopBreak::None
        );
    }

    #[test]
    fn exact_repeat_polling_then_stall_triggers() {
        // Polling with changing results, then result stabilizes → should trigger.
        let mut lb = default_breaker();
        let args = r#"{"command": "gh run view 123"}"#;
        assert_eq!(
            lb.record_and_check("shell", args, "status: in_progress"),
            LoopBreak::None
        );
        assert_eq!(
            lb.record_and_check("shell", args, "status: in_progress, 2 jobs running"),
            LoopBreak::None
        );
        // Now the result stabilizes (same result repeated).
        assert_eq!(
            lb.record_and_check("shell", args, "status: completed, success"),
            LoopBreak::None
        );
        assert_eq!(
            lb.record_and_check("shell", args, "status: completed, success"),
            LoopBreak::None
        );
        // Third identical call: now triggers.
        match lb.record_and_check("shell", args, "status: completed, success") {
            LoopBreak::Detected(LoopBreakReason::ExactRepeat { tool, count, .. }) => {
                assert_eq!(tool, "shell");
                assert_eq!(count, 3);
            }
            other => panic!("expected ExactRepeat after stall, got {:?}", other),
        }
    }

    // ── Ping-pong ──────────────────────────────────────────────────────────

    #[test]
    fn ping_pong_triggers_after_four_rounds() {
        // Verify that call 8 (end of 4th round) triggers ping-pong
        // when using a config with ping_pong_rounds: 4.
        let config = LoopBreakerConfig {
            ping_pong_rounds: 4,
            ..LoopBreakerConfig::default()
        };
        let mut lb = LoopBreaker::new(config);
        for i in 0..8 {
            let tool = if i % 2 == 0 { "read_file" } else { "write_file" };
            let result = lb.record_and_check(tool, "{}", "data");
            if i < 7 {
                assert_eq!(result, LoopBreak::None, "call {} should not trigger", i + 1);
            } else {
                match result {
                    LoopBreak::Detected(LoopBreakReason::PingPong { rounds, .. }) => {
                        assert_eq!(rounds, 4);
                    }
                    other => panic!("expected PingPong on call 8, got {:?}", other),
                }
            }
        }
    }

    #[test]
    fn ping_pong_not_triggered_for_same_tool() {
        let mut lb = default_breaker();
        for _ in 0..10 {
            let result = lb.record_and_check("read_file", "{}", "data");
            // Same tool repeated — should not be ping-pong (it's an exact repeat instead).
            if let LoopBreak::Detected(LoopBreakReason::PingPong { .. }) = result {
                panic!("same tool should not trigger ping-pong");
            }
        }
    }

    #[test]
    fn ping_pong_broken_by_third_tool() {
        let mut lb = default_breaker();
        // 3 rounds of alternating.
        for _ in 0..3 {
            lb.record_and_check("read_file", "{}", "data");
            lb.record_and_check("write_file", "{}", "ok");
        }
        // A third tool breaks the pattern.
        let result = lb.record_and_check("list_files", "{}", "ok");
        assert_eq!(result, LoopBreak::None);
    }

    // ── No progress ────────────────────────────────────────────────────────

    #[test]
    fn no_progress_triggers_when_same_result_different_args() {
        let mut lb = default_breaker();
        // Call "search" 5 times with different args but same result.
        for i in 0..5 {
            let args = format!(r#"{{"query": "attempt {}"}}"#, i);
            let result = lb.record_and_check("search", &args, "no results found");
            if i < 4 {
                assert_eq!(result, LoopBreak::None, "call {} should not trigger", i + 1);
            } else {
                match result {
                    LoopBreak::Detected(LoopBreakReason::NoProgress { tool, count }) => {
                        assert_eq!(tool, "search");
                        assert_eq!(count, 5);
                    }
                    other => panic!("expected NoProgress, got {:?}", other),
                }
            }
        }
    }

    #[test]
    fn no_progress_broken_by_different_tool() {
        let mut lb = default_breaker();
        // 4 "search" calls with same result — not enough to trigger (threshold = 5).
        for i in 0..4 {
            let args = format!(r#"{{"query": "attempt {}"}}"#, i);
            let result = lb.record_and_check("search", &args, "no results found");
            assert_eq!(result, LoopBreak::None);
        }
        // Insert a different tool — breaks the consecutive streak.
        lb.record_and_check("read_file", "{}", "data");
        // 4 more "search" calls — still not enough, streak was reset.
        for i in 0..4 {
            let args = format!(r#"{{"query": "attempt2 {}"}}"#, i);
            let result = lb.record_and_check("search", &args, "no results found");
            assert_eq!(result, LoopBreak::None, "should not trigger after streak break");
        }
    }

    #[test]
    fn no_progress_relaxed_tool_uses_higher_threshold() {
        // "shell" is in the default relaxed_tools list, so it needs 2x threshold (10).
        let mut lb = default_breaker();
        // 9 calls with same result — should NOT trigger for relaxed tool.
        for i in 0..9 {
            let args = format!(r#"{{"command": "grep pattern{} file"}}"#, i);
            let result = lb.record_and_check("shell", &args, "exit code: 0");
            assert_eq!(result, LoopBreak::None, "relaxed tool: call {} should not trigger", i + 1);
        }
        // 10th call — triggers with relaxed threshold.
        match lb.record_and_check("shell", r#"{"command": "grep pattern10 file"}"#, "exit code: 0") {
            LoopBreak::Detected(LoopBreakReason::NoProgress { tool, count }) => {
                assert_eq!(tool, "shell");
                assert_eq!(count, 10);
            }
            other => panic!("expected NoProgress for shell at relaxed threshold, got {:?}", other),
        }
    }

    #[test]
    fn no_progress_broken_by_different_result() {
        let mut lb = default_breaker();
        // 4 same results, then a different result, then more same — streak resets.
        for i in 0..4 {
            let args = format!(r#"{{"query": "attempt {}"}}"#, i);
            let result = lb.record_and_check("search", &args, "no results found");
            assert_eq!(result, LoopBreak::None);
        }
        // Different result breaks the streak.
        lb.record_and_check("search", r#"{"query": "lucky"}"#, "found something!");
        // 4 more same results — still not enough.
        for i in 0..4 {
            let args = format!(r#"{{"query": "attempt2 {}"}}"#, i);
            let result = lb.record_and_check("search", &args, "no results found");
            assert_eq!(result, LoopBreak::None);
        }
    }

    #[test]
    fn no_progress_not_triggered_when_results_differ() {
        let mut lb = default_breaker();
        for i in 0..6 {
            let args = format!(r#"{{"query": "attempt {}"}}"#, i);
            let result_text = format!("result number {}", i);
            // Different results each time → no "no progress" trigger.
            let result = lb.record_and_check("search", &args, &result_text);
            // May trigger exact repeat if args happen to hash same, but they won't
            // since each args string is different.
            if let LoopBreak::Detected(LoopBreakReason::NoProgress { .. }) = result {
                panic!("should not trigger NoProgress with different results");
            }
        }
    }

    // ── Max calls ──────────────────────────────────────────────────────────

    #[test]
    fn max_calls_triggers_hard_limit() {
        let config = LoopBreakerConfig {
            max_tool_calls: 5,
            ..LoopBreakerConfig::default()
        };
        let mut lb = LoopBreaker::new(config);

        for i in 0..5 {
            assert_eq!(
                lb.record_and_check("tool", &format!("{}", i), &format!("result {}", i)),
                LoopBreak::None,
                "call {} should not trigger",
                i + 1
            );
        }
        // 6th call exceeds limit.
        match lb.record_and_check("tool", "6", "result 6") {
            LoopBreak::Detected(LoopBreakReason::MaxCalls { count, limit }) => {
                assert_eq!(count, 6);
                assert_eq!(limit, 5);
            }
            other => panic!("expected MaxCalls, got {:?}", other),
        }
    }

    #[test]
    fn max_calls_zero_means_unlimited() {
        let config = LoopBreakerConfig {
            max_tool_calls: 0,
            ..LoopBreakerConfig::default()
        };
        let mut lb = LoopBreaker::new(config);

        // Make 50 calls — should never trigger MaxCalls.
        for i in 0..50 {
            let args = format!(r#"{{"n": {}}}"#, i);
            if let LoopBreak::Detected(LoopBreakReason::MaxCalls { .. }) = lb.record_and_check("tool", &args, "ok") {
                panic!("should not trigger MaxCalls when max_tool_calls=0");
            }
        }
        assert_eq!(lb.total_calls(), 50);
    }

    // ── Reset ──────────────────────────────────────────────────────────────

    #[test]
    fn reset_clears_state() {
        let mut lb = default_breaker();
        lb.record_and_check("tool", "{}", "ok");
        lb.record_and_check("tool", "{}", "ok");
        assert_eq!(lb.total_calls(), 2);

        lb.reset();
        assert_eq!(lb.total_calls(), 0);

        // After reset, should not have accumulated state.
        assert_eq!(lb.record_and_check("tool", "{}", "ok"), LoopBreak::None);
    }

    // ── Sliding window ─────────────────────────────────────────────────────

    #[test]
    fn sliding_window_respects_max_size() {
        let config = LoopBreakerConfig {
            window_size: 5,
            ..LoopBreakerConfig::default()
        };
        let mut lb = LoopBreaker::new(config);

        // Make 10 calls with different args — window should cap at 5.
        for i in 0..10 {
            let args = format!(r#"{{"n": {}}}"#, i);
            lb.record_and_check("tool", &args, "ok");
        }
        assert_eq!(lb.total_calls(), 10);
        // Internal window should be at most 5. We can't inspect it directly,
        // but we can verify that exact-repeat doesn't trigger with different args.
        // The window only has the last 5 entries.
    }

    // ── Simple hash determinism ────────────────────────────────────────────

    #[test]
    fn simple_hash_is_deterministic() {
        let h1 = simple_hash("hello world");
        let h2 = simple_hash("hello world");
        assert_eq!(h1, h2);
    }

    #[test]
    fn simple_hash_differs_for_different_input() {
        let h1 = simple_hash("hello");
        let h2 = simple_hash("world");
        assert_ne!(h1, h2);
    }

    #[test]
    fn simple_hash_handles_empty_string() {
        let h = simple_hash("");
        // Just ensure it doesn't panic and produces something.
        assert_ne!(h, 0);
    }

    #[test]
    fn simple_hash_is_full_content() {
        // Full hash should differ for strings that share head+tail but differ in the middle.
        let long_a = format!("{}middle_A{}", "a".repeat(200), "z".repeat(200));
        let long_b = format!("{}middle_B{}", "a".repeat(200), "z".repeat(200));
        assert_ne!(simple_hash(&long_a), simple_hash(&long_b));
    }
}
