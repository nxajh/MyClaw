//! @提及 预解析（入站）与 `<ref>` 输出渲染（出站）。
//!
//! RFC §2.2（身份模型重构后）：
//! - **入站**：自由文本 `@username` → 原位替换为 `<ref id="myclaw/u/<uuidv7>"/>`
//!   进 agent 上下文（gate 之后、进 agent 之前统一解析；解析失败 → 框架模板
//!   回复，零 token）。`username` 全局唯一（UserRegistry 全表精确解析，不限
//!   关系；大小写宽容）。不再支持 `@u/uid` 形态——uid 是系统分配的 uuidv7，
//!   不可读、不可作为用户输入。邮箱中的 `@`（`a@b.com`）跳过。好友校验不在
//!   此层——「你们还不是好友」由 send_message 工具内 contacts 检查拦截。
//! - **出站**：agent 输出 `<ref id="myclaw/u/<uuidv7>"/>` → `@username`
//!   （白名单解析：只处理 `<ref id="…"/>`，属性必须双引号、自闭合；其余
//!   `<…>` 原样保留；username 查不到（含旧数据空串）→ 原样保留标签——uid
//!   不可读，不渲染 `@u/<uuid>`；`<namespace>/` 前缀不符 → 原样保留标签）。

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

/// 单个 `<ref id="…"/>` 的渲染：验 `<namespace>/u/` 前缀 → registry 实时
/// username。查到 → `@username`；查不到（含旧数据空串）→ 原样保留标签
/// （uid 不可读，不渲染 `@u/<uuid>`）；前缀不符 → 原样保留标签。
fn render_one_ref(id: &str, registry: &UserRegistry) -> String {
    if !id.starts_with(&format!("{}/u/", registry.namespace())) {
        return format!("<ref id=\"{id}\"/>");
    }
    match registry.username_of(id) {
        Some(username) => format!("@{username}"),
        None => format!("<ref id=\"{id}\"/>"),
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

/// 扫描入站自由文本中的 @提及（RFC §2.2「消息 @提及」：`@username` 全局
/// 精确解析，结构判定，无试探）：
/// - `@username`：UserRegistry 全表精确解析（username 唯一），不限关系；
///   未找到 → Failed。
///
/// 命中 → 原位替换 `<ref id="{fqid}"/>`。
/// 邮箱防御：`@` 前一字符为邮箱 local-part 合法字符（如 `a@b.com`）→ 跳过。
pub fn resolve_mentions(text: &str, registry: &UserRegistry) -> MentionResolution {
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
        match resolve_one(token, registry) {
            MentionResolution::Resolved(repl) => {
                out.push_str(&repl);
                rest = &after[token.len()..];
            }
            fail @ MentionResolution::Failed(_) => return fail,
        }
    }
    // 循环只消费到最后一个提及；剩余尾部文本（提及后的内容）必须保留。
    out.push_str(rest);
    MentionResolution::Resolved(out)
}

/// 单个 token 的解析（`@username`：全局精确解析，username 唯一）。
fn resolve_one(token: &str, registry: &UserRegistry) -> MentionResolution {
    match registry.find_by_username(token) {
        Some(user) => MentionResolution::Resolved(format!(
            "<ref id=\"{}\"/>",
            user.user_id(registry.namespace())
        )),
        None => MentionResolution::Failed(not_found_reply(token)),
    }
}

/// 未找到模板（RFC §2.2 入站解析失败 → 框架模板回复，零 token）。
fn not_found_reply(token: &str) -> String {
    format!(
        "未找到 @{token}，ta 尚未与本 bot 互动，无法通知；可让 ta 先发条消息或 /register。"
    )
}

/// 取 @ 后的提及 token：直到空白或常见标点（保留 `_` 与字母数字——
/// username 规则 `[a-z0-9_]{3,32}`；`.` 终止——`@alice.` 句号结尾常见）。
fn take_mention_token(s: &str) -> &str {
    let end = s
        .char_indices()
        .find(|(_, c)| {
            c.is_whitespace()
                || "，。！？!?,.;:()'\"（）《》、【】[]<>~`|\\^=+*&#%/-".contains(*c)
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
        r.register("alice@example.com", "alice").unwrap();
        r.register("bob@example.com", "bob").unwrap();
        r.register("carol@example.com", "carol").unwrap();
        r
    }

    fn id_of(r: &UserRegistry, username: &str) -> String {
        r.find_by_username(username).unwrap().user_id(r.namespace())
    }

    // ── render_refs ─────────────────────────────────────────────────────────

    #[test]
    fn render_refs_username_and_fallback() {
        let r = registry();
        let alice = id_of(&r, "alice");
        let carol = id_of(&r, "carol");
        // 有 username → @username
        assert_eq!(
            render_refs(&format!("通知 <ref id=\"{alice}\"/> 开会"), &r),
            "通知 @alice 开会"
        );
        assert_eq!(
            render_refs(&format!("hi <ref id=\"{carol}\"/>"), &r),
            "hi @carol"
        );
        // 查不到（含旧数据 username 空串）→ 原样保留标签（uid 不可读）。
        assert_eq!(
            render_refs("<ref id=\"myclaw/u/ghost\"/>", &r),
            "<ref id=\"myclaw/u/ghost\"/>"
        );
    }

    #[test]
    fn render_refs_whitelist_preserves_other_tags() {
        let r = registry();
        let alice = id_of(&r, "alice");
        // 非 ref 标签原样保留
        assert_eq!(
            render_refs(&format!("<b>粗体</b> <ref id=\"{alice}\"/>"), &r),
            "<b>粗体</b> @alice"
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
        let alice = id_of(&r, "alice");
        let mut rr = RefRenderer::new();
        assert_eq!(rr.push(&format!("通知 <ref id=\"{alice}"), &r), "通知 ");
        assert_eq!(rr.push("\"/> 开会", &r), "@alice 开会");
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

    #[test]
    fn resolve_username_global_unlimited_by_relationship() {
        let r = registry();
        let carol = id_of(&r, "carol");
        // @carol 不限关系：carol 不是任何人的好友也能解析（全局全表查）。
        let MentionResolution::Resolved(out) = resolve_mentions("通知 @carol 开会", &r) else {
            panic!("expected resolved");
        };
        assert_eq!(out, format!("通知 <ref id=\"{carol}\"/> 开会"));
    }

    #[test]
    fn resolve_username_case_insensitive() {
        let r = registry();
        let alice = id_of(&r, "alice");
        let MentionResolution::Resolved(out) = resolve_mentions("找 @ALICE 开会", &r) else {
            panic!("expected resolved");
        };
        assert_eq!(out, format!("找 <ref id=\"{alice}\"/> 开会"));
    }

    #[test]
    fn resolve_username_not_found() {
        let r = registry();
        match resolve_mentions("@ghost 你好", &r) {
            MentionResolution::Failed(reply) => {
                assert!(reply.contains("未找到 @ghost"), "reply: {reply}");
            }
            _ => panic!("expected failed"),
        }
    }

    #[test]
    fn resolve_skips_email_at_sign() {
        let r = registry();
        let alice = id_of(&r, "alice");
        // `a@b.com` 中的 @ 不是提及
        let MentionResolution::Resolved(out) = resolve_mentions("联系 a@b.com 或 @alice", &r)
        else {
            panic!("expected resolved");
        };
        assert_eq!(out, format!("联系 a@b.com 或 <ref id=\"{alice}\"/>"));
    }

    #[test]
    fn resolve_multiple_mentions_in_one_message() {
        let r = registry();
        let carol = id_of(&r, "carol");
        let bob = id_of(&r, "bob");
        let MentionResolution::Resolved(out) =
            resolve_mentions("@carol 和 @bob 都通知", &r)
        else {
            panic!("expected resolved");
        };
        assert_eq!(
            out,
            format!("<ref id=\"{carol}\"/> 和 <ref id=\"{bob}\"/> 都通知")
        );
    }

    #[test]
    fn resolve_username_token_terminated_by_punctuation() {
        let r = registry();
        let bob = id_of(&r, "bob");
        // 中文标点终止 token
        let MentionResolution::Resolved(out) = resolve_mentions("通知@bob，开会", &r) else {
            panic!("expected resolved");
        };
        assert_eq!(out, format!("通知<ref id=\"{bob}\"/>，开会"));
    }
}
