//! `/register` / `/email` / `/nickname` slash commands — P4 用户自服务
//! (RFC §2.2; deterministic, bypasses the LLM).
//!
//! `/register <邮箱> <uid>` 创建用户实体并绑定当前渠道；`/email set <邮箱>`
//! 更换邮箱（唯一、可更换）；`/nickname set <昵称>` 设置显示昵称（可重复、
//! 不允许含 `/`，显示层实时渲染）。昵称不落联系人快照——好友列表、待处理
//! 请求、消息回执一律经 [`UserRegistry::display`] 实时取。
//!
//! [`parse_target`] 是好友命令、`/link`、工具层共用的目标解析：把用户输入
//! （`u/uid`、完整 user.id、或邮箱）解析为内部统一键 user.id（FQID）。

use crate::agents::commands::CommandContext;
use crate::agents::{RegisterError, UserRegistry};

/// 解析目标参数（`u/uid`、完整 user.id、或邮箱）为完整 user.id（FQID）。
///
/// 好友命令、`/link`、工具层共用（P4 第一波：只支持 id/email；`@昵称`
/// 解析属第二波，这里给出明确报错而不是静默误判）。目标必须已注册。
pub(crate) fn parse_target(user_registry: &UserRegistry, arg: &str) -> Result<String, String> {
    let arg = arg.trim();
    if arg.is_empty() {
        return Err("目标不能为空（用 u/uid 或邮箱，如 u/alice）".to_string());
    }
    // `u/uid` 或完整 user.id（`<ns>/u/<uid>`）。uid 本身只允许小写，前缀
    // 与句柄统一归一化小写以宽容大小写输入。
    let lower = arg.to_lowercase();
    let uid = lower
        .strip_prefix("u/")
        .or_else(|| lower.strip_prefix(&format!("{}/u/", user_registry.namespace())));
    if let Some(uid) = uid {
        let uid = uid.trim();
        if uid.is_empty() {
            return Err("目标不能为空（用 u/uid 或邮箱，如 u/alice）".to_string());
        }
        if let Some(user) = user_registry.find_by_uid(uid) {
            return Ok(user.user_id(user_registry.namespace()));
        }
        return Err(format!(
            "未找到用户 u/{uid}（对方尚未注册身份，无法作为目标；让对方先 /register）"
        ));
    }
    // `@` 前缀 = @提及 形态（`@昵称`/`@u/uid`），属 P4 第二波，明确报错。
    if arg.starts_with('@') {
        return Err(
            "暂不支持 @昵称 解析（P4 第二波接入），请用 u/uid 或邮箱，如 u/alice".to_string(),
        );
    }
    if arg.contains('@') {
        if let Some(user) = user_registry.find_by_email(arg) {
            return Ok(user.user_id(user_registry.namespace()));
        }
        return Err(format!(
            "未找到邮箱 {arg} 对应的用户（对方尚未注册身份，无法作为目标）"
        ));
    }
    Err(format!(
        "无法识别的目标“{arg}”（用 u/uid 或邮箱，如 u/alice）"
    ))
}

/// `/register <邮箱> <uid>` — 创建用户身份并绑定当前渠道。
///
/// 成功后把当前 routing_key 通过 resolver 绑定到新 FQID（gate 据此放行），
/// 并把该 rk 名下既有的联系人/邮箱数据折叠到新身份。
pub fn cmd_register(args: &str, ctx: CommandContext<'_>) -> String {
    let mut parts = args.split_whitespace();
    let email = parts.next().unwrap_or("");
    let uid = parts.next().unwrap_or("");
    if email.is_empty() || uid.is_empty() {
        return "用法: /register <邮箱> <uid>（uid 为 3–32 位小写字母/数字/下划线，如 alice）"
            .to_string();
    }
    // 已注册 → 拒绝重复注册（防止覆盖绑定产生孤儿用户）。
    let self_uid = ctx.known_users.resolve_uid(ctx.user_id);
    if ctx.user_registry.is_user_id(&self_uid) {
        return format!(
            "当前账号已注册为 {}。修改邮箱用 /email set，昵称用 /nickname set。",
            ctx.user_registry.display(&self_uid)
        );
    }
    let nickname = parts.next();
    match ctx.user_registry.register(email, uid, nickname) {
        Ok(user) => {
            let fqid = user.user_id(ctx.user_registry.namespace());
            // 绑定当前 rk → FQID：gate 放行 + 联系人/邮箱键折叠。
            if let Some(resolver) = ctx.known_users.resolver() {
                resolver.set(ctx.user_id, fqid.clone());
            }
            ctx.known_users.migrate_identity(ctx.user_id, &fqid);
            format!(
                "✅ 身份创建成功：{}。好友将用此 id 找到你；可用 /email set 换邮箱、/nickname set 设置昵称。",
                ctx.user_registry.display(&fqid)
            )
        }
        Err(e) => err_text(&e),
    }
}

/// `/email set <邮箱>`（或直接 `/email <邮箱>`）— 更换邮箱（唯一、可更换）。
pub fn cmd_email(args: &str, ctx: CommandContext<'_>) -> String {
    let mut it = args.split_whitespace();
    let first = it.next().unwrap_or("");
    let email = if first.eq_ignore_ascii_case("set") {
        it.next().unwrap_or("")
    } else {
        first
    };
    if email.is_empty() {
        return "用法: /email set <邮箱>".to_string();
    }
    let Some(uid) = current_uid(&ctx) else {
        return "当前账号尚未注册身份，请先 /register <邮箱> <uid>".to_string();
    };
    match ctx.user_registry.set_email(&uid, email) {
        Ok(()) => format!("✅ 邮箱已更新为 {email}"),
        Err(e) => err_text(&e),
    }
}

/// `/nickname set <昵称>`（或直接 `/nickname <昵称>`）— 设置显示昵称。
/// 昵称可重复、不允许含 `/`；显示层实时渲染，不落联系人快照。
pub fn cmd_nickname(args: &str, ctx: CommandContext<'_>) -> String {
    let trimmed = args.trim();
    // "set " 前缀形式优先；裸昵称直接整体作为昵称（允许含空格）。
    let nick = match trimmed.strip_prefix("set ") {
        Some(rest) => rest.trim(),
        None => trimmed,
    };
    if nick.is_empty() {
        return "用法: /nickname set <昵称>（不允许含 /）".to_string();
    }
    let Some(uid) = current_uid(&ctx) else {
        return "当前账号尚未注册身份，请先 /register <邮箱> <uid>".to_string();
    };
    match ctx.user_registry.set_nickname(&uid, nick) {
        Ok(()) => format!(
            "✅ 昵称已设为 {}（显示为 {}）",
            nick,
            ctx.user_registry.display(&ctx.user_registry.user_id_of(&uid))
        ),
        Err(e) => err_text(&e),
    }
}

/// 当前账号绑定的 uid（已注册时）。`None` = 未注册。
fn current_uid(ctx: &CommandContext<'_>) -> Option<String> {
    let self_uid = ctx.known_users.resolve_uid(ctx.user_id);
    ctx.user_registry.uid_of(&self_uid).map(|s| s.to_string())
}

/// RegisterError → 用户可见文案。
fn err_text(e: &RegisterError) -> String {
    match e {
        RegisterError::InvalidUid(m)
        | RegisterError::ReservedUid(m)
        | RegisterError::UidTaken(m)
        | RegisterError::InvalidEmail(m)
        | RegisterError::EmailTaken(m)
        | RegisterError::InvalidNickname(m)
        | RegisterError::NoSuchUser(m) => format!("❌ {m}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_target_resolves_uid_email_and_fqid() {
        let reg = UserRegistry::in_memory();
        reg.register("alice@example.com", "alice", Some("小艾"))
            .unwrap();
        // u/uid 形态（大小写宽容）。
        assert_eq!(parse_target(&reg, "u/alice").unwrap(), "myclaw/u/alice");
        assert_eq!(parse_target(&reg, "U/ALICE").unwrap(), "myclaw/u/alice");
        // 完整 FQID。
        assert_eq!(
            parse_target(&reg, "myclaw/u/alice").unwrap(),
            "myclaw/u/alice"
        );
        // email（大小写不敏感）。
        assert_eq!(
            parse_target(&reg, "Alice@Example.com").unwrap(),
            "myclaw/u/alice"
        );
    }

    #[test]
    fn parse_target_rejects_unknown_and_second_wave_forms() {
        let reg = UserRegistry::in_memory();
        reg.register("alice@example.com", "alice", Some("小艾"))
            .unwrap();
        assert!(parse_target(&reg, "u/nobody").unwrap_err().contains("未找到"));
        assert!(parse_target(&reg, "bob@example.com")
            .unwrap_err()
            .contains("未找到"));
        // @昵称 = P4 第二波，明确报错。
        assert!(parse_target(&reg, "@小艾").unwrap_err().contains("第二波"));
        assert!(parse_target(&reg, "bob").unwrap_err().contains("无法识别"));
        assert!(parse_target(&reg, "").unwrap_err().contains("不能为空"));
    }

    #[test]
    fn register_roundtrip_email_nickname_updates() {
        let reg = UserRegistry::in_memory();
        let user = reg.register("alice@example.com", "alice", None).unwrap();
        assert_eq!(user.user_id("myclaw"), "myclaw/u/alice");
        assert_eq!(reg.find_by_uid("alice").unwrap().email.as_deref(), Some("alice@example.com"));
        // 换邮箱：旧邮箱释放、新邮箱占用。
        reg.set_email("alice", "new@example.com").unwrap();
        assert!(reg.find_by_email("alice@example.com").is_none());
        assert_eq!(reg.find_by_email("new@example.com").unwrap().uid, "alice");
        // 设置昵称 → 显示形态。
        reg.set_nickname("alice", "小艾").unwrap();
        assert_eq!(reg.display("myclaw/u/alice"), "@小艾(u/alice)");
        // 重复注册被拒。
        assert!(matches!(
            reg.register("other@example.com", "alice", None),
            Err(RegisterError::UidTaken(_))
        ));
    }
}
