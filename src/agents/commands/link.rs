//! `/link` + `/link_confirm` slash commands — P3 身份绑定 (user channel,
//! deterministic, bypasses the LLM).
//!
//! Flow: a user on a new channel claims to be an existing registered user
//! (`/link u/username` 或邮箱). The framework pushes a one-time verification code to
//! the *claimed* account as a framework template (zero LLM tokens). Only
//! the holder of that account can see the code; replying
//! `/link_confirm <code>` from the new channel proves control of both
//! channels. The framework then folds the two routing_keys into one
//! user_id via the shared `UserResolver` (RFC §2/P3: 好友、消息、记忆按
//! "人"共享).
//!
//! Security bounds: 6-digit code, 10-minute TTL, 3 wrong attempts
//! invalidate the attempt; you cannot link to yourself; an already-linked
//! channel refuses further links (no unlink yet). Pending attempts live
//! in process memory only — a daemon restart just requires re-running
//! `/link`.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::agents::commands::friends::{notify_peer, rk_for};
use crate::agents::commands::register::parse_target;
use crate::agents::commands::CommandContext;

/// 6-digit one-time code lifetime.
const LINK_TTL_MS: u64 = 10 * 60 * 1000;
/// Wrong attempts before the pending link is invalidated.
const LINK_MAX_ATTEMPTS: u32 = 3;

/// What a successful confirm resolves to: the folded identity the current
/// routing_key merges into, plus the claimed account's routing key (for
/// the confirmation notification back on the old channel).
#[derive(Debug)]
struct LinkTarget {
    uid: String,
    rk: String,
}

struct PendingLink {
    code: String,
    target_rk: String,
    target_uid: String,
    expires_ms: u64,
    attempts: u32,
}

/// In-process pending link attempts, keyed by the initiator's routing_key.
/// `LazyLock` because `HashMap::new` is not const (cannot live in a plain
/// `static Mutex` initializer).
static PENDING_LINKS: LazyLock<Mutex<HashMap<String, PendingLink>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Generate a 6-digit numeric code.
fn generate_code() -> String {
    format!("{:06}", rand::random::<u32>() % 1_000_000)
}

/// Register a new pending link for `current_rk` and return the code to
/// deliver. A previous pending attempt by the same user is overwritten
/// (the new code supersedes it).
fn start_link(current_rk: &str, target_rk: &str, target_uid: &str, now: u64) -> String {
    let code = generate_code();
    PENDING_LINKS.lock().unwrap().insert(
        current_rk.to_string(),
        PendingLink {
            code: code.clone(),
            target_rk: target_rk.to_string(),
            target_uid: target_uid.to_string(),
            expires_ms: now + LINK_TTL_MS,
            attempts: 0,
        },
    );
    code
}

/// Validate one confirm attempt against the stored pending link and, on
/// success, clear it. On failure, updates the attempt counter (killing the
/// attempt when exhausted or expired) and returns the user-facing error.
fn consume_confirm(current_rk: &str, code: &str, now: u64) -> Result<LinkTarget, String> {
    let mut pending = PENDING_LINKS.lock().unwrap();
    let Some(link) = pending.get_mut(current_rk) else {
        return Err("没有待确认的关联请求，请先 /link u/uid 发起".to_string());
    };
    if now > link.expires_ms {
        pending.remove(current_rk);
        return Err("验证码已过期，请重新 /link u/uid".to_string());
    }
    if link.attempts >= LINK_MAX_ATTEMPTS {
        pending.remove(current_rk);
        return Err("错误次数过多，本次关联已作废，请重新 /link u/uid".to_string());
    }
    if code != link.code {
        link.attempts += 1;
        if link.attempts >= LINK_MAX_ATTEMPTS {
            pending.remove(current_rk);
            return Err("验证码错误次数过多，本次关联已作废，请重新 /link u/uid".to_string());
        }
        let left = LINK_MAX_ATTEMPTS - link.attempts;
        return Err(format!("验证码错误（还剩 {left} 次机会），或重新 /link 获取新验证码"));
    }
    let target = LinkTarget {
        uid: link.target_uid.clone(),
        rk: link.target_rk.clone(),
    };
    pending.remove(current_rk);
    Ok(target)
}

/// `/link u/uid 或邮箱` — claim to be an existing registered user from a new
/// channel. The verification code is pushed to one of the target user's live
/// channels; confirming proves control of both channels.
pub async fn cmd_link(args: &str, ctx: CommandContext<'_>) -> String {
    let target = match parse_target(ctx.user_registry, args) {
        Ok(t) => t,
        Err(e) => return e,
    };
    let Some(resolver) = ctx.known_users.resolver() else {
        return "当前部署未启用身份绑定".to_string();
    };
    // 当前渠道已绑定 → 同身份幂等成功，异身份拒绝（暂无解绑）。
    let bound = resolver.resolve(ctx.user_id);
    if bound != ctx.user_id {
        if bound == target {
            return format!(
                "当前账号已关联到 {}，无需重复关联",
                ctx.user_registry.display(&target)
            );
        }
        return "当前账号已绑定到其他身份，暂不支持重复绑定或解绑".to_string();
    }
    // 目标的可投递渠道：resolver 绑定渠道优先，其次登记簿。
    let Some(target_rk) = rk_for(ctx.known_users, &target) else {
        return format!(
            "目标 {} 当前没有可投递的渠道，无法发送验证码，请稍后再试",
            ctx.user_registry.display(&target)
        );
    };
    let code = start_link(ctx.user_id, &target_rk, &target, now_ms());
    let me = {
        let uid = ctx.known_users.resolve_uid(ctx.user_id);
        ctx.user_registry.display(&uid)
    };
    // 验证码走框架模板直达目标渠道（零 LLM token）；失败 = 目标不可达。
    let sent = notify_peer(
        &ctx,
        &target_rk,
        &format!(
            "🔐 {me} 正在尝试把新渠道关联到你的账号。验证码：{code}（10 分钟内有效）。若不是你本人操作，请忽略。"
        ),
    )
    .await;
    if !sent {
        PENDING_LINKS.lock().unwrap().remove(ctx.user_id);
        return "目标用户所在渠道当前不可达，无法发送验证码，请稍后再试".to_string();
    }
    format!(
        "验证码已发送到 {}（{}），请查看后回复 /link_confirm 验证码",
        ctx.user_registry.display(&target),
        target_rk
    )
}

/// `/link_confirm 验证码` — confirm the pending link with the code shown
/// on the claimed account. On success folds the two identities.
pub async fn cmd_link_confirm(args: &str, ctx: CommandContext<'_>) -> String {
    let code = args.trim();
    if code.is_empty() {
        return "用法: /link_confirm 验证码（6 位数字）".to_string();
    }
    let target = match consume_confirm(ctx.user_id, code, now_ms()) {
        Ok(t) => t,
        Err(e) => return e,
    };
    let Some(resolver) = ctx.known_users.resolver() else {
        return "当前部署未启用身份绑定".to_string();
    };
    // 绑定：routing_key → 折叠 user_id（resolver 内部落盘），迁移旧身份数据。
    resolver.set(ctx.user_id, &target.uid);
    ctx.known_users.migrate_identity(ctx.user_id, &target.uid);
    // 回执旧渠道（best effort；渠道离线则静默跳过）。
    let _ = notify_peer(
        &ctx,
        &target.rk,
        &format!(
            "✅ 你的账号已关联新渠道（{}）。现在两个渠道共享好友、消息与记忆。",
            ctx.channel_type
        ),
    )
    .await;
    format!(
        "✅ 关联成功：当前渠道与 {} 已合并为同一身份（{}）。好友、消息与记忆将在两个渠道间共享。",
        ctx.user_registry.display(&target.uid),
        target.uid
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_is_six_digits() {
        for _ in 0..100 {
            let c = generate_code();
            assert_eq!(c.len(), 6);
            assert!(c.chars().all(|ch| ch.is_ascii_digit()));
        }
    }

    #[test]
    fn start_then_confirm_roundtrip() {
        let now = 1_000_000;
        let code = start_link("telegram:default:me", "qqbot:xiaoer:me", "qqbot:xiaoer:me", now);
        let target = consume_confirm("telegram:default:me", &code, now).unwrap();
        assert_eq!(target.uid, "qqbot:xiaoer:me");
        assert_eq!(target.rk, "qqbot:xiaoer:me");
        // Consumed: a second confirm finds nothing pending.
        assert!(consume_confirm("telegram:default:me", &code, now).is_err());
    }

    #[test]
    fn confirm_without_pending_fails() {
        let err = consume_confirm("nobody:default:x", "123456", 0).unwrap_err();
        assert!(err.contains("/link"));
    }

    #[test]
    fn wrong_code_counts_attempts_then_kills() {
        let now = 5_000;
        let code = start_link("a:default:x", "b:default:y", "b:default:y", now);
        for i in 0..LINK_MAX_ATTEMPTS {
            let wrong = if code == "000000" { "000001" } else { "000000" };
            let err = consume_confirm("a:default:x", wrong, now).unwrap_err();
            if i + 1 < LINK_MAX_ATTEMPTS {
                assert!(err.contains("还剩"), "attempt {i}: {err}");
            } else {
                assert!(err.contains("次数过多"), "attempt {i}: {err}");
            }
        }
        // Even the correct code is refused once the attempt is dead.
        assert!(consume_confirm("a:default:x", &code, now).is_err());
    }

    #[test]
    fn expired_link_is_rejected_and_cleared() {
        let now = 1_000;
        let code = start_link("exp:default:x", "b:default:y", "b:default:y", now);
        let err = consume_confirm("exp:default:x", &code, now + LINK_TTL_MS + 1).unwrap_err();
        assert!(err.contains("过期"), "{err}");
        assert!(consume_confirm("exp:default:x", &code, now + LINK_TTL_MS + 1).is_err());
    }
}
