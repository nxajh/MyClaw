use super::*;

#[test]
fn record_creates_and_updates() {
    let reg = KnownUsersRegistry::in_memory();
    reg.record("qqbot", "xiaoer", "user_a", "c2c");
    assert_eq!(reg.count(), 1);
    assert_eq!(reg.total_messages(), 1);

    reg.record("qqbot", "xiaoer", "user_a", "c2c");
    assert_eq!(reg.count(), 1);
    assert_eq!(reg.total_messages(), 2);
}

#[test]
fn check_and_record_respects_sender_limit() {
    let reg = KnownUsersRegistry::in_memory();
    for _ in 0..30 {
        assert!(
            reg.check_and_record("qqbot", "xiaoer", "spammer", "c2c"),
            "should allow within sender limit"
        );
    }
    assert!(
        !reg.check_and_record("qqbot", "xiaoer", "spammer", "c2c"),
        "should block after sender limit exceeded"
    );
    assert!(
        reg.check_and_record("qqbot", "xiaoer", "user_b", "c2c"),
        "different sender should be allowed"
    );
}

#[test]
fn check_and_record_respects_global_limit() {
    let reg = KnownUsersRegistry::in_memory();
    for i in 0..300 {
        assert!(
            reg.check_and_record("qqbot", "xiaoer", &format!("user_{i}"), "c2c"),
            "should allow within global limit (sender {i})"
        );
    }
    assert!(
        !reg.check_and_record("qqbot", "xiaoer", "user_300", "c2c"),
        "should block after global limit exceeded"
    );
}

#[test]
fn users_for_filters_by_channel_account() {
    let reg = KnownUsersRegistry::in_memory();
    reg.record("qqbot", "xiaoer", "user_a", "c2c");
    reg.record("qqbot", "xiaosan", "user_b", "c2c");
    reg.record("telegram", "default", "user_c", "c2c");

    let xiaoer = reg.users_for("qqbot", "xiaoer");
    assert_eq!(xiaoer.len(), 1);
    assert_eq!(xiaoer[0].user_id, "user_a");

    let all = reg.all_users();
    assert_eq!(all.len(), 3);
}

#[test]
fn flush_round_trip() {
    let dir = std::env::temp_dir();
    let path = dir.join(format!(
        "myclaw_test_known_users_{}.json",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);

    let reg = KnownUsersRegistry::new(&dir);
    // Override path to our test file.
    // KnownUsersRegistry::new always uses known_users.json, so we
    // test via the public flush + manual load path.
    reg.record("qqbot", "xiaoer", "user_a", "c2c");
    reg.record("telegram", "default", "12345", "c2c");
    reg.flush();

    // Load from the same file.
    let expected_path = dir.join("known_users.json");
    let contents = std::fs::read_to_string(&expected_path).unwrap();
    let file: PersistedFile = serde_json::from_str(&contents).unwrap();
    assert_eq!(file.users.len(), 2);

    let _ = std::fs::remove_file(&expected_path);
}

// ── Contacts (RFC §4) ───────────────────────────────────────────────────

fn alice() -> &'static str {
    "qqbot:xiaoer:alice"
}
fn bob() -> &'static str {
    "qqbot:xiaoer:bob"
}

#[test]
fn request_accept_delivery_flow() {
    let reg = KnownUsersRegistry::in_memory();
    assert_eq!(reg.request_friend(alice(), bob()), RequestOutcome::New);

    // Bob sees one pending inbound request.
    let pending = reg.pending_requests(bob());
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].0, alice());
    assert_eq!(pending[0].1.status, ContactStatus::Pending);
    assert_eq!(pending[0].1.direction, ContactDirection::In);

    // Not friends yet → delivery intercepted.
    assert_eq!(
        reg.delivery_verdict(alice(), bob()),
        DeliveryVerdict::NotFriends
    );

    // Accept → both sides mirror to Accepted.
    assert!(reg.accept_friend(bob(), alice()));
    assert_eq!(
        reg.delivery_verdict(alice(), bob()),
        DeliveryVerdict::Allowed
    );
    assert_eq!(
        reg.delivery_verdict(bob(), alice()),
        DeliveryVerdict::Allowed
    );

    // Accepting a non-pending pair fails.
    assert!(!reg.accept_friend(alice(), bob()));
}

#[test]
fn request_declined_cooldown_blocks_repeat() {
    let reg = KnownUsersRegistry::in_memory();
    assert_eq!(reg.request_friend(alice(), bob()), RequestOutcome::New);
    assert!(reg.decline_friend(bob(), alice()));

    // Re-request within 24h → refused.
    assert_eq!(
        reg.request_friend(alice(), bob()),
        RequestOutcome::DeclinedTooSoon
    );
    assert_eq!(
        reg.delivery_verdict(alice(), bob()),
        DeliveryVerdict::NotFriends
    );
}

#[test]
fn request_blocked_by_peer() {
    let reg = KnownUsersRegistry::in_memory();
    reg.block_friend(bob(), alice());

    assert_eq!(
        reg.request_friend(alice(), bob()),
        RequestOutcome::BlockedByPeer
    );
    assert_eq!(
        reg.delivery_verdict(alice(), bob()),
        DeliveryVerdict::Blocked
    );

    // Unblock returns to no-relationship → fresh request allowed.
    assert!(reg.unblock_friend(bob(), alice()));
    assert_eq!(reg.request_friend(alice(), bob()), RequestOutcome::New);
}

#[test]
fn request_pending_is_idempotent() {
    let reg = KnownUsersRegistry::in_memory();
    assert_eq!(reg.request_friend(alice(), bob()), RequestOutcome::New);
    assert_eq!(
        reg.request_friend(alice(), bob()),
        RequestOutcome::AlreadyPending
    );
    assert_eq!(reg.pending_requests(bob()).len(), 1);
}

#[test]
fn remove_friend_breaks_delivery_both_ways() {
    let reg = KnownUsersRegistry::in_memory();
    reg.request_friend(alice(), bob());
    reg.accept_friend(bob(), alice());

    assert!(reg.remove_friend(bob(), alice()));
    assert_eq!(
        reg.delivery_verdict(alice(), bob()),
        DeliveryVerdict::NotFriends
    );
    assert_eq!(
        reg.delivery_verdict(bob(), alice()),
        DeliveryVerdict::NotFriends
    );
    assert!(!reg.remove_friend(bob(), alice()));
}

#[test]
fn user_mailbox_drains_once() {
    let reg = KnownUsersRegistry::in_memory();
    reg.push_user_mail(
        bob(),
        UserMail {
            msg_id: "m1".into(),
            sender_user_id: alice().into(),
            sender_nickname: "@alice".into(),
            text: "hello".into(),
            sent_at: 1,
        },
    );
    reg.push_user_mail(
        bob(),
        UserMail {
            msg_id: "m2".into(),
            sender_user_id: alice().into(),
            sender_nickname: "@alice".into(),
            text: "again".into(),
            sent_at: 2,
        },
    );

    let drained = reg.drain_user_mail(bob());
    assert_eq!(drained.len(), 2);
    assert_eq!(drained[0].text, "hello");
    // Inject-once: second drain is empty.
    assert!(reg.drain_user_mail(bob()).is_empty());
}

#[test]
fn render_user_mail_reminder_lists_all_mails() {
    let mails = vec![
        UserMail {
            msg_id: "m1".into(),
            sender_user_id: alice().into(),
            sender_nickname: "@alice".into(),
            text: "你好".into(),
            sent_at: 1,
        },
        UserMail {
            msg_id: "m2".into(),
            sender_user_id: alice().into(),
            sender_nickname: "@alice".into(),
            text: "在吗".into(),
            sent_at: 2,
        },
    ];
    let rendered = KnownUsersRegistry::render_user_mail_reminder(&mails);
    assert!(rendered.contains("<system-reminder>"), "{rendered}");
    assert!(rendered.contains("2 条来自好友的消息"), "{rendered}");
    assert!(rendered.contains("来自 @alice 的消息"), "{rendered}");
    assert!(rendered.contains("你好"), "{rendered}");
    assert!(rendered.contains("在吗"), "{rendered}");
}

#[test]
fn render_pending_requests_reminder_lists_each_request() {
    let reg = KnownUsersRegistry::in_memory();
    reg.request_friend(alice(), bob());
    let pending = reg.pending_requests(bob());
    assert_eq!(pending.len(), 1);
    // display 闭包由调用方注入（P4: 实时昵称渲染）。
    let rendered =
        KnownUsersRegistry::render_pending_requests_reminder(&pending, |_| "alice".to_string());
    assert!(rendered.contains("<system-reminder>"), "{rendered}");
    assert!(rendered.contains("待处理好友请求"), "{rendered}");
    assert!(rendered.contains("alice"), "{rendered}");
    // No pending → empty render list, no reminder text.
    let rendered_empty = KnownUsersRegistry::render_pending_requests_reminder(&[], |_| String::new());
    assert!(rendered_empty.contains("共有 0 条"), "{rendered_empty}");
}

#[test]
fn last_seen_ms_of_tracks_user_activity() {
    // RFC §6 P2 会话发现: last_seen 数据源。
    let reg = KnownUsersRegistry::in_memory();
    assert!(reg.last_seen_ms_of(alice()).is_none());
    reg.record("qqbot", "xiaoer", "alice", "c2c");
    assert!(reg.last_seen_ms_of(alice()).unwrap() > 0);
}

#[test]
fn render_presence_labels_online_recent_offline() {
    let now = now_ms();
    // Fresh interaction → online.
    let online = KnownUsersRegistry::render_presence(now);
    assert!(online.contains("🟢"), "{online}");
    assert!(online.contains("在线"), "{online}");
    // ~10 minutes ago → recently active.
    let recent = KnownUsersRegistry::render_presence(now - 10 * 60_000);
    assert!(recent.contains("🟡"), "{recent}");
    assert!(recent.contains("最近活跃"), "{recent}");
    // 3 days ago → offline.
    let offline = KnownUsersRegistry::render_presence(now - 3 * 86_400_000);
    assert!(offline.contains("⚪"), "{offline}");
    assert!(offline.contains("离线"), "{offline}");
}

#[test]
fn render_user_mail_reminder_includes_reply_guidance() {
    // RFC §6 P2 回复转发闭环: 注入文本带回复引导, 接收方 agent 知道如何回。
    let mails = vec![UserMail {
        msg_id: "m1".into(),
        sender_user_id: alice().into(),
        sender_nickname: "@alice".into(),
        text: "你好".into(),
        sent_at: 1,
    }];
    let rendered = KnownUsersRegistry::render_user_mail_reminder(&mails);
    assert!(rendered.contains("send_message"), "{rendered}");
    assert!(rendered.contains("recipient=u/"), "{rendered}");
}

#[test]
fn contacts_and_mailbox_persist_round_trip() {
    let dir = std::env::temp_dir().join(format!(
        "myclaw_contacts_test_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let expected_path = dir.join("known_users.json");

    let reg = KnownUsersRegistry::new(&dir);
    reg.request_friend(alice(), bob());
    reg.accept_friend(bob(), alice());
    reg.push_user_mail(
        bob(),
        UserMail {
            msg_id: "m1".into(),
            sender_user_id: alice().into(),
            sender_nickname: "@alice".into(),
            text: "persisted".into(),
            sent_at: 1,
        },
    );
    reg.flush();

    let contents = std::fs::read_to_string(&expected_path).unwrap();
    let file: PersistedFile = serde_json::from_str(&contents).unwrap();
    assert!(file.contacts.contains_key(alice()));
    assert!(file.user_mailbox.contains_key(bob()));

    let _ = std::fs::remove_dir_all(&dir);
}

// ── P3 identity folding ──────────────────────────────────────────────────

fn carol() -> &'static str {
    "qqbot:xiaoer:carol"
}

#[test]
fn with_resolver_folds_contacts_and_mailbox() {
    let reg = KnownUsersRegistry::in_memory();
    let resolver = Arc::new(UserResolver::new());
    resolver.set("telegram:default:alice_tg", alice());
    let reg = reg.with_resolver(resolver);

    // 从新渠道（telegram rk）发起好友请求 → 关系折叠到 alice 身份。
    assert_eq!(
        reg.request_friend("telegram:default:alice_tg", bob()),
        RequestOutcome::New
    );
    assert_eq!(reg.list_contacts(alice()).len(), 1);
    // bob 侧回执也折叠: 接受来自折叠身份的请求。
    assert!(reg.accept_friend(bob(), "telegram:default:alice_tg"));
    assert_eq!(
        reg.delivery_verdict("telegram:default:alice_tg", bob()),
        DeliveryVerdict::Allowed
    );
    assert_eq!(
        reg.delivery_verdict(bob(), "telegram:default:alice_tg"),
        DeliveryVerdict::Allowed
    );
    // mailbox 键折叠: 投递到 bob 任意渠道都命中。
    reg.push_user_mail(
        bob(),
        UserMail {
            msg_id: "m1".into(),
            sender_user_id: "telegram:default:alice_tg".into(),
            sender_nickname: "@alice_tg".into(),
            text: "hi".into(),
            sent_at: 1,
        },
    );
    assert_eq!(reg.drain_user_mail(bob()).len(), 1);
}

#[test]
fn migrate_identity_merges_mailbox_and_contacts() {
    let reg = KnownUsersRegistry::in_memory();
    let old_rk = "telegram:default:alice_tg";
    // 绑定前: old_rk 与 bob 已是好友；carol 对 old_rk 有 pending 请求；
    // mailbox 里有一条投递给 old_rk 的消息。
    assert_eq!(reg.request_friend(old_rk, bob()), RequestOutcome::New);
    assert!(reg.accept_friend(bob(), old_rk));
    assert_eq!(reg.request_friend(carol(), old_rk), RequestOutcome::New);
    reg.push_user_mail(
        old_rk,
        UserMail {
            msg_id: "m1".into(),
            sender_user_id: bob().into(),
            sender_nickname: "@bob".into(),
            text: "hello".into(),
            sent_at: 1,
        },
    );

    reg.migrate_identity(old_rk, alice());

    // old_rk 的 owner 维度 → alice（bob accepted + carol pending in）。
    let contacts = reg.list_contacts(alice());
    assert_eq!(contacts.len(), 2);
    assert!(
        contacts
            .iter()
            .any(|(p, e)| p == bob() && e.status == ContactStatus::Accepted)
    );
    assert!(
        contacts
            .iter()
            .any(|(p, e)| p == carol() && e.status == ContactStatus::Pending)
    );
    assert!(reg.list_contacts(old_rk).is_empty());
    // carol 侧 peer 键 → alice（折叠身份作为联系人键，实时显示名）。
    let carol_contacts = reg.list_contacts(carol());
    assert!(carol_contacts.iter().any(|(p, _)| p == alice()));
    assert!(!carol_contacts.iter().any(|(p, _)| p == old_rk));
    // mailbox 合并。
    assert_eq!(reg.drain_user_mail(alice()).len(), 1);
    // 幂等 no-op（不 panic）。
    reg.migrate_identity(old_rk, alice());
}

#[test]
fn migrate_identity_noop_when_same() {
    let reg = KnownUsersRegistry::in_memory();
    reg.migrate_identity(alice(), alice());
    assert!(reg.list_contacts(alice()).is_empty());
}

#[test]
fn last_seen_ms_of_folds_across_channels() {
    let reg = KnownUsersRegistry::in_memory();
    let resolver = Arc::new(UserResolver::new());
    resolver.set("telegram:default:alice_tg", alice());
    let reg = reg.with_resolver(resolver);
    reg.users.insert(
        "telegram:default:alice_tg".to_string(),
        KnownUser {
            channel: "telegram".into(),
            account: "default".into(),
            user_id: "alice_tg".into(),
            message_count: 1,
            first_seen_ms: 1,
            last_seen_ms: 2000,
            scope: "c2c".into(),
        },
    );
    reg.users.insert(
        alice().to_string(),
        KnownUser {
            channel: "qqbot".into(),
            account: "xiaoer".into(),
            user_id: "alice".into(),
            message_count: 1,
            first_seen_ms: 1,
            last_seen_ms: 1000,
            scope: "c2c".into(),
        },
    );
    // 折叠身份取所有渠道最新；未绑定 rk 直接查自身。
    assert_eq!(reg.last_seen_ms_of(alice()), Some(2000));
    assert_eq!(reg.last_seen_ms_of("telegram:default:alice_tg"), Some(2000));
    // 未注册用户 → None。
    assert_eq!(reg.last_seen_ms_of(bob()), None);
}
