//! P4 第二波：@提及 预解析（入站）与 `<ref>` 输出渲染（出站）。
//!
//! RFC §2.2：
//! - **入站**：自由文本 `@昵称` / `@u/uid` → 原位替换为 `<ref id="myclaw/u/…"/>`
//!   进 agent 上下文（gate 之后、进 agent 之前统一解析；解析失败 → 框架模板回复，
//!   零 token）。`@` 后以 `u/` 开头 → UserRegistry 精确解析（不限关系）；否则 →
//!   已建立关系（Accepted）内实时昵称比对，多命中拦截询问，绝不猜测。邮箱中的
//!   `@`（`a@b.com`）跳过。好友校验不在此层——「你们还不是好友」由 send_message
//!   工具内 contacts 检查拦截。
//! - **出站**：agent 输出 `<ref id="myclaw/u/alice"/>` → `@昵称(u/alice)`（白名单
//!   解析：只处理 `<ref id="…"/>`，属性必须双引号、自闭合；其余 `<…>` 原样保留；
//!   查不到 → `@u/alice`；`<namespace>/` 前缀不符 → 原样保留标签）。

use super::known_users::{ContactStatus, KnownUsersRegistry};
use super::user_registry::UserRegistry;

// ── 出站渲染 ─────────────────────────────────────────────────────────────────

/// 输出渲染（非流式整段）。见模块注释。聊天回复与 send_message 正文的
/// 用户可见形态都经过这里（send_message 正文注入对方 agent 上下文时保持
/// `<ref>` 原样——agent 只见 id 标记）。
pub fn render_refs(text: &str, registry: &UserRegistry) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let Some(open) = rest.find("<ref id=\"") else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..open]);
        let after = &rest[open + "<ref id=\"".len()..];
        let Some(close_quote) = after.find('"') else {
            // 属性值未闭合——按普通文本原样保留（不炸）。
            out.push_str(&rest[open..]);
            break;
        };
        let id = &after[..close_quote];
        let tail = &after[close_quote + 1..];
        match tail.strip_prefix("/>") {
            Some(rest_of_tail) => {
                out.push_str(&render_one_ref(id, registry));
                rest = rest_of_tail;
            }
            None => {
                // 属性后不是 `/>`（如 `<ref id="x">…</ref>`）→ 整段原样保留。
                out.push_str(&rest[open..]);
                break;
            }
        }
    }
    out
}

/// 单个 `<ref id="…"/>` 的渲染：验 `<namespace>/u/` 前缀 → registry 实时昵称。
/// 有昵称 → `@昵称(u/uid)`；无昵称/查不到 → `@u/uid`；前缀不符 → 原样保留标签。
fn render_one_ref(id: &str, registry: &UserRegistry) -> String {
    if !id.starts_with(&format!("{}/u/", registry.namespace())) {
        return format!("<ref id=\"{id}\"/>");
    }
    // display(): 有昵称 → `@昵称(u/uid)`；无昵称/查不到 → `u/uid`（补 @ 兜底）。
    let disp = registry.display(id);
    if disp.starts_with('@') {
        disp
    } else {
        format!("@{disp}")
    }
}

/// 流式输出渲染器：跨 chunk 缓冲未闭合的 `<ref …` 前缀，避免标签被流式分块
/// 切断（LLM 流式输出可能在任何字节位置切块）。
#[derive(Default)]
pub struct RefRenderer {
    pending: String,
}

impl RefRenderer {
    pub fn new() -> Self {
        Self::default()
    }

    /// 处理一个流式 chunk，返回可推送的渲染文本（可能为空——全部暂存中）。
    pub fn push(&mut self, delta: &str, registry: &UserRegistry) -> String {
        self.pending.push_str(delta);
        // 尾部是未闭合的 `<ref …` 前缀 → 暂存，等后续 chunk 补全后再渲染。
        if let Some(lt) = self.pending.rfind('<') {
            let tail = &self.pending[lt..];
            if !tail.contains('>') && tail.starts_with("<ref") {
                let head = self.pending[..lt].to_string();
                self.pending = tail.to_string();
                return render_refs(&head, registry);
            }
        }
        let rendered = render_refs(&self.pending, registry);
        self.pending.clear();
        rendered
    }

    /// 流结束：输出剩余（未闭合的前缀原样保留——不渲染残缺标签）。
    pub fn flush(&mut self) -> String {
        std::mem::take(&mut self.pending)
    }
}

// ── 入站 @提及 解析 ──────────────────────────────────────────────────────────

/// 入站 @提及 解析结果。
pub enum MentionResolution {
    /// 全部解析成功：替换后的文本（进 agent 上下文）。
    Resolved(String),
    /// 解析失败：模板回复文本（框架层直接回复，不进 agent，零 token）。
    Failed(String),
}

/// 扫描入站自由文本中的 @提及（RFC §2.2「消息 @提及」两形态，结构判定，无试探）：
/// - `@u/uid`：UserRegistry 精确解析，不限关系；未找到 → Failed。
/// - `@昵称`：仅限已建立关系内（Accepted 好友）实时昵称比对；0 命中 → Failed；
///   多命中 → Failed（「有多个用户，请给出唯一标识」，绝不猜测）。
///
/// 命中 → 原位替换 `<ref id="{fqid}"/>`。
/// 邮箱防御：`@` 前一字符为邮箱 local-part 合法字符（如 `a@b.com`）→ 跳过。
pub fn resolve_mentions(
    text: &str,
    owner: &str,
    known: &KnownUsersRegistry,
    registry: &UserRegistry,
) -> MentionResolution {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('@') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        // 邮箱防御：`@` 前一字符是邮箱 local-part 合法字符 → 不是提及。
        if at > 0 && is_email_local_char(rest.as_bytes()[at - 1] as char) {
            out.push('@');
            rest = after;
            continue;
        }
        let token = take_mention_token(after);
        if token.is_empty() {
            out.push('@');
            rest = after;
            continue;
        }
        match resolve_one(token, owner, known, registry) {
            MentionResolution::Resolved(repl) => {
                out.push_str(&repl);
                rest = &after[token.len()..];
            }
            fail @ MentionResolution::Failed(_) => return fail,
        }
    }
    MentionResolution::Resolved(out)
}

/// 单个 token 的解析（结构判定：`@` 后以 `u/` 开头 → id；否则 → 昵称）。
fn resolve_one(
    token: &str,
    owner: &str,
    known: &KnownUsersRegistry,
    registry: &UserRegistry,
) -> MentionResolution {
    if let Some(uid) = token.strip_prefix("u/") {
        // id 形态：UserRegistry 精确解析，不限关系。
        if registry.find_by_uid(uid).is_some() {
            MentionResolution::Resolved(format!("<ref id=\"{}\"/>", registry.user_id_of(uid)))
        } else {
            MentionResolution::Failed(not_found_reply(token))
        }
    } else {
        // 昵称形态：仅限已建立关系内（Accepted）实时昵称比对。
        let hits: Vec<String> = known
            .list_contacts(owner)
            .into_iter()
            .filter(|(_, entry)| entry.status == ContactStatus::Accepted)
            .map(|(peer, _)| peer)
            .filter(|peer| current_nick(peer, registry) == token)
            .collect();
        match hits.len() {
            1 => MentionResolution::Resolved(format!("<ref id=\"{}\"/>", hits[0])),
            0 => MentionResolution::Failed(not_found_reply(token)),
            _ => MentionResolution::Failed(format!(
                "@{token} 有多个用户，请给出唯一标识（u/uid 或邮箱）。"
            )),
        }
    }
}

/// 未找到模板（RFC §2.2 入站解析失败 → 框架模板回复，零 token）。
fn not_found_reply(token: &str) -> String {
    format!(
        "未找到 @{token}，ta 尚未与本 bot 互动，无法通知；可让 ta 先发条消息或 /register。"
    )
}

/// 实时昵称（RFC §2.2：昵称不落快照）：用户自设昵称，未设置回退派生（uid 句柄）。
fn current_nick(peer: &str, registry: &UserRegistry) -> String {
    match registry.nickname_of(peer) {
        Some(nick) => nick,
        None => registry.uid_of(peer).map(str::to_string).unwrap_or_default(),
    }
}

/// 取 @ 后的提及 token：直到空白或常见标点（保留 `/`、`-`、`_` 与中文等
/// 非 ascii 字符——昵称可含中文；`/` 仅出现在 `u/uid` 形态；`.` 终止——
/// `@alice.` 句号结尾常见，昵称校验未禁 `.` 但 token 终止更稳）。
fn take_mention_token(s: &str) -> &str {
    let end = s
        .char_indices()
        .find(|(_, c)| {
            c.is_whitespace()
                || "，。！？!?,.;:()'\"（）《》、【】[]<>~`|\\^=+*&#%".contains(*c)
        })
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    &s[..end]
}

/// 邮箱 local-part 合法字符（防御 `a@b.com` 中的 @ 被误判为提及）。
fn is_email_local_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '%' | '+' | '-')
}

// ── 测试 ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> UserRegistry {
        let r = UserRegistry::in_memory();
        r.register("alice@example.com", "alice", None).unwrap();
        r.register("bob@example.com", "bob", Some("小艾")).unwrap();
        r.register("carol@example.com", "carol", None).unwrap();
        r.set_nickname("alice", "爱丽丝").unwrap();
        r
    }

    // ── render_refs ─────────────────────────────────────────────────────────

    #[test]
    fn render_refs_nickname_and_fallback() {
        let r = registry();
        // 有昵称 → @昵称(u/uid)
        assert_eq!(
            render_refs("通知 <ref id=\"myclaw/u/alice\"/> 开会", &r),
            "通知 @爱丽丝(u/alice) 开会"
        );
        // 无昵称 → @u/uid 兜底
        assert_eq!(
            render_refs("hi <ref id=\"myclaw/u/carol\"/>", &r),
            "hi @u/carol"
        );
        // 查不到 → @u/uid
        assert_eq!(
            render_refs("<ref id=\"myclaw/u/ghost\"/>", &r),
            "@u/ghost"
        );
    }

    #[test]
    fn render_refs_whitelist_preserves_other_tags() {
        let r = registry();
        // 非 ref 标签原样保留
        assert_eq!(
            render_refs("<b>粗体</b> <ref id=\"myclaw/u/alice\"/>", &r),
            "<b>粗体</b> @爱丽丝(u/alice)"
        );
        // 单引号属性不匹配（裸写不炸，当普通文本）
        assert_eq!(
            render_refs("<ref id='myclaw/u/alice'/>", &r),
            "<ref id='myclaw/u/alice'/>"
        );
        // 非自闭合（`<ref id="x">…</ref>`）整段保留
        assert_eq!(
            render_refs("<ref id=\"myclaw/u/alice\">x</ref>", &r),
            "<ref id=\"myclaw/u/alice\">x</ref>"
        );
        // 外部 namespace 前缀不符 → 原样保留标签
        assert_eq!(
            render_refs("a <ref id=\"evil/u/alice\"/> b", &r),
            "a <ref id=\"evil/u/alice\"/> b"
        );
    }

    #[test]
    fn render_refs_no_refs_passthrough() {
        let r = registry();
        assert_eq!(render_refs("普通文本 <b>hi</b> 2 < 3", &r), "普通文本 <b>hi</b> 2 < 3");
    }

    // ── RefRenderer（流式跨 chunk） ─────────────────────────────────────────

    #[test]
    fn ref_renderer_joins_split_tag() {
        let r = registry();
        let mut rr = RefRenderer::new();
        assert_eq!(rr.push("通知 <ref id=\"myclaw/u/al", &r), "通知 ");
        assert_eq!(rr.push("ice\"/> 开会", &r), "@爱丽丝(u/alice) 开会");
        assert_eq!(rr.flush(), "");
    }

    #[test]
    fn ref_renderer_flush_keeps_unclosed_prefix() {
        let r = registry();
        let mut rr = RefRenderer::new();
        assert_eq!(rr.push("残 <ref id=\"myclaw/u/ali", &r), "残 ");
        assert_eq!(rr.flush(), "<ref id=\"myclaw/u/ali");
    }

    #[test]
    fn ref_renderer_plain_chunks_flow_through() {
        let r = registry();
        let mut rr = RefRenderer::new();
        assert_eq!(rr.push("你好，", &r), "你好，");
        assert_eq!(rr.push("世界", &r), "世界");
        assert_eq!(rr.flush(), "");
    }

    // ── resolve_mentions（入站） ────────────────────────────────────────────

    fn known_with_friends() -> (KnownUsersRegistry, String, String) {
        let known = KnownUsersRegistry::in_memory();
        let owner = "myclaw/u/alice".to_string();
        let peer = "myclaw/u/bob".to_string();
        // peer → owner 请求，owner 接受（owner 名下 peer 为 Accepted）。
        known.request_friend(&peer, &owner);
        known.accept_friend(&owner, &peer);
        (known, owner, peer)
    }

    #[test]
    fn resolve_id_form_unlimited_by_relationship() {
        let r = registry();
        let (known, owner, _) = known_with_friends();
        // @u/uid 不限关系：carol 不是好友也能解析
        let MentionResolution::Resolved(out) =
            resolve_mentions("通知 @u/carol 开会", &owner, &known, &r)
        else {
            panic!("expected resolved");
        };
        assert_eq!(out, "通知 <ref id=\"myclaw/u/carol\"/> 开会");
    }

    #[test]
    fn resolve_id_form_not_found() {
        let r = registry();
        let (known, owner, _) = known_with_friends();
        match resolve_mentions("@u/ghost 你好", &owner, &known, &r) {
            MentionResolution::Failed(reply) => {
                assert!(reply.contains("未找到 @u/ghost"), "reply: {reply}");
            }
            _ => panic!("expected failed"),
        }
    }

    #[test]
    fn resolve_nickname_within_relationship() {
        let r = registry();
        let (known, owner, _) = known_with_friends();
        // bob 昵称「小艾」；alice 与 bob 是好友 → 命中
        let MentionResolution::Resolved(out) =
            resolve_mentions("通知 @小艾 下午3点开会", &owner, &known, &r)
        else {
            panic!("expected resolved");
        };
        assert_eq!(out, "通知 <ref id=\"myclaw/u/bob\"/> 下午3点开会");
    }

    #[test]
    fn resolve_nickname_falls_back_to_uid() {
        let r = registry();
        let (known, owner, _) = known_with_friends();
        // carol 无昵称 → 派生 uid 比对；但 carol 非好友 → 未找到（关系内才解析）
        match resolve_mentions("@carol 你好", &owner, &known, &r) {
            MentionResolution::Failed(reply) => assert!(reply.contains("未找到 @carol")),
            _ => panic!("expected failed"),
        }
    }

    #[test]
    fn resolve_nickname_multi_hit_asks_for_disambiguation() {
        let r = registry();
        let (known, owner, _) = known_with_friends();
        // 两个好友同名昵称
        let peer2 = "myclaw/u/carol".to_string();
        known.request_friend(&peer2, &owner);
        known.accept_friend(&owner, &peer2);
        r.set_nickname("carol", "小艾").unwrap();
        match resolve_mentions("通知 @小艾 开会", &owner, &known, &r) {
            MentionResolution::Failed(reply) => {
                assert!(reply.contains("有多个用户"), "reply: {reply}");
                assert!(reply.contains("唯一标识"), "reply: {reply}");
            }
            _ => panic!("expected failed"),
        }
    }

    #[test]
    fn resolve_skips_email_at_sign() {
        let r = registry();
        let (known, owner, _) = known_with_friends();
        // `a@b.com` 中的 @ 不是提及
        let MentionResolution::Resolved(out) =
            resolve_mentions("联系 a@b.com 或 @u/alice", &owner, &known, &r)
        else {
            panic!("expected resolved");
        };
        assert_eq!(out, "联系 a@b.com 或 <ref id=\"myclaw/u/alice\"/>");
    }

    #[test]
    fn resolve_multiple_mentions_in_one_message() {
        let r = registry();
        let (known, owner, _) = known_with_friends();
        let MentionResolution::Resolved(out) = resolve_mentions(
            "@u/carol 和 @小艾 都通知",
            &owner,
            &known,
            &r,
        ) else {
            panic!("expected resolved");
        };
        assert_eq!(
            out,
            "<ref id=\"myclaw/u/carol\"/> 和 <ref id=\"myclaw/u/bob\"/> 都通知"
        );
    }

    #[test]
    fn resolve_chinese_nickname_token() {
        let r = registry();
        let (known, owner, _) = known_with_friends();
        // 中文昵称 + 中文标点终止 token
        let MentionResolution::Resolved(out) =
            resolve_mentions("通知@小艾，开会", &owner, &known, &r)
        else {
            panic!("expected resolved");
        };
        assert_eq!(out, "通知<ref id=\"myclaw/u/bob\"/>，开会");
    }
}
