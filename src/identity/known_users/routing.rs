use super::registry::KnownUsersRegistry;

/// Build the canonical routing key: `channel:account:user_id`.
pub(super) fn routing_key(channel: &str, account: &str, user_id: &str) -> String {
    format!("{channel}:{account}:{user_id}")
}

/// 把折叠身份（user.id FQID）映射到可投递的 routing_key（RFC §4.3 通知用）。
///
/// 优先 resolver 里绑定的渠道（`/register`、`/link` 都会绑定）；其次在
/// 登记簿里找 resolve 到该身份的 rk。找不到（目标无任何渠道）返回 None。
pub(crate) fn rk_for(known_users: &KnownUsersRegistry, user_id: &str) -> Option<String> {
    if let Some(resolver) = known_users.resolver() {
        let keys = resolver.routing_keys_for(user_id);
        if let Some(k) = keys.first() {
            return Some(k.clone());
        }
    }
    for u in known_users.all_users() {
        let rk = format!("{}:{}:{}", u.channel, u.account, u.user_id);
        if known_users.resolve_uid(&rk) == user_id {
            return Some(rk);
        }
    }
    None
}
