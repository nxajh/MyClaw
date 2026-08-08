//! UserRegistry — P4 用户实体层（uid / email / nickname），与渠道登记簿分离。
//!
//! RFC §2.2: 每个用户是独立的 `User` 实体，由用户自选句柄 `uid` 标识
//! （`[a-z0-9_]{3,32}`、保留字不可用、先到先得、不可变）。`email` 唯一且
//! 可更换；`nickname` 可重复、不允许含 `/`。渠道侧 routing_key 到用户的
//! 映射仍由 [`UserResolver`] 承担（值统一为 user.id，如 `myclaw/u/alice`）；
//! 本注册表只存"人"的实体属性，不感知渠道。
//!
//! 持久化在 `{data_dir}/users.json`，每次变更写盘（数据量小，与
//! `user_resolver.json` 同一策略）。命名空间默认 `myclaw`（RFC:
//! `messaging.namespace` 配置项，后续接入）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::agents::{KnownUsersRegistry, UserResolver};

// ── 常量与规则 ───────────────────────────────────────────────────────────────

/// 保留字（uid 不可用）：root 及系统级保留（RFC §2.2）。
pub const RESERVED_UIDS: &[&str] = &["root", "admin", "system", "bot", "help", "register"];
/// uid 长度下限。
pub const UID_MIN: usize = 3;
/// uid 长度上限。
pub const UID_MAX: usize = 32;
/// 昵称最大长度。
pub const NICK_MAX: usize = 32;
/// 默认命名空间（RFC: messaging.namespace，配置项后续接入）。
pub const DEFAULT_NAMESPACE: &str = "myclaw";
/// 存量用户归入的根 uid（RFC §2.2 迁移：存量 identity 全归 root）。
pub const ROOT_UID: &str = "root";

/// 由 uid 构造完整 user.id（三段式 FQID：`<namespace>/u/<uid>`）。
pub fn user_id(namespace: &str, uid: &str) -> String {
    format!("{namespace}/u/{uid}")
}

// ── 用户实体 ─────────────────────────────────────────────────────────────────

/// 一个用户（P4 用户实体层）。`uid` 是不可变自选句柄；`email` 唯一可更换；
/// `nickname` 可重复、不允许含 `/`（RFC §2.2 结构判定: `@` 后以 `u/` 开头
/// 按 id 精确解析，否则按昵称在关系内实时比对——昵称含 `/` 会破坏判定）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    /// 自选句柄（`[a-z0-9_]{3,32}`，不含 `u/` 前缀）。
    pub uid: String,
    /// 唯一邮箱（小写归一化；未验证状态下也先占位）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// 显示昵称（可重复；`None` = 未设置，显示层回退 `u/uid`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nickname: Option<String>,
    /// 是否激活（D1：User 仅在 /register 或 /link 成功时创建为 active）。
    pub active: bool,
    /// 创建时间（unix ms）。
    pub created_ms: u64,
}

impl User {
    /// 完整 user.id（`myclaw/u/<uid>`）。
    pub fn user_id(&self, namespace: &str) -> String {
        user_id(namespace, &self.uid)
    }
}

#[derive(Serialize, Deserialize)]
struct PersistedUsers {
    version: u32,
    users: HashMap<String, User>,
}

/// 注册/更新失败原因（用户可见文案）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterError {
    /// uid 格式非法（字符/长度）。
    InvalidUid(String),
    /// uid 为保留字。
    ReservedUid(String),
    /// uid 已被占用（先到先得）。
    UidTaken(String),
    /// email 格式非法。
    InvalidEmail(String),
    /// email 已被占用（唯一）。
    EmailTaken(String),
    /// nickname 非法（为空/超长/含 `/`）。
    InvalidNickname(String),
    /// 用户不存在（更新类操作）。
    NoSuchUser(String),
}

// ── 注册表 ───────────────────────────────────────────────────────────────────

/// 用户实体注册表：`uid → User` + `email → uid` 索引，持久化 `users.json`。
#[derive(Debug)]
pub struct UserRegistry {
    users: RwLock<HashMap<String, User>>,
    /// email（小写）→ uid 唯一索引。
    email_index: RwLock<HashMap<String, String>>,
    /// 持久化路径（空 = 内存态，不落盘）。
    data_path: PathBuf,
    /// 命名空间（user.id 前缀，默认 `myclaw`）。
    namespace: String,
}

impl UserRegistry {
    /// 内存态注册表（测试 / CLI 模式）。
    pub fn in_memory() -> Self {
        Self {
            users: RwLock::new(HashMap::new()),
            email_index: RwLock::new(HashMap::new()),
            data_path: PathBuf::new(),
            namespace: DEFAULT_NAMESPACE.to_string(),
        }
    }

    /// 持久化注册表（`{data_dir}/users.json`），默认命名空间 `myclaw`。
    pub fn persistent(data_dir: &Path) -> Self {
        Self::with_namespace(data_dir, DEFAULT_NAMESPACE)
    }

    /// 持久化注册表 + 自定义命名空间（RFC: messaging.namespace 配置项接入点）。
    pub fn with_namespace(data_dir: &Path, namespace: &str) -> Self {
        let data_path = data_dir.join("users.json");
        let reg = Self {
            users: RwLock::new(HashMap::new()),
            email_index: RwLock::new(HashMap::new()),
            data_path: data_path.clone(),
            namespace: namespace.to_string(),
        };
        reg.load_from_disk();
        reg
    }

    /// 命名空间（user.id 前缀段）。
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// uid → 完整 user.id。
    pub fn user_id_of(&self, uid: &str) -> String {
        user_id(&self.namespace, uid)
    }

    /// 判断一个字符串是否为本实例的合法 user.id（`<ns>/u/<uid>` 形态）。
    pub fn is_user_id(&self, s: &str) -> bool {
        s.starts_with(&format!("{}/u/", self.namespace))
            && s.len() > self.namespace.len() + 3
    }

    /// 从 user.id 提取 uid 句柄（非本实例 id 返回 None）。
    pub fn uid_of(&self, user_id_str: &str) -> Option<&str> {
        user_id_str.strip_prefix(&format!("{}/u/", self.namespace))
    }

    /// 用户可见显示形态（RFC §2.2 显示层）：
    /// 有昵称 → `@昵称(u/uid)`；无昵称 → `u/uid`；非本实例 id 原样返回。
    pub fn display(&self, user_id_str: &str) -> String {
        let Some(uid) = self.uid_of(user_id_str) else {
            return user_id_str.to_string();
        };
        let visible = format!("u/{uid}");
        match self.users.read().get(uid).and_then(|u| u.nickname.clone()) {
            Some(nick) => format!("@{nick}({visible})"),
            None => visible,
        }
    }

    /// 实时昵称（RFC §2.2: 昵称不落快照，显示/比对一律实时取）。
    pub fn nickname_of(&self, user_id_str: &str) -> Option<String> {
        let uid = self.uid_of(user_id_str)?;
        self.users.read().get(uid).and_then(|u| u.nickname.clone())
    }

    // ── 查询 ────────────────────────────────────────────────────────────────

    /// 按 uid 查询（uid 不带 `u/` 前缀）。
    pub fn find_by_uid(&self, uid: &str) -> Option<User> {
        self.users.read().get(uid).cloned()
    }

    /// 按邮箱查询（大小写不敏感——内部小写归一化）。
    pub fn find_by_email(&self, email: &str) -> Option<User> {
        let email = normalize_email(email);
        let uid = self.email_index.read().get(&email)?.clone();
        self.users.read().get(&uid).cloned()
    }

    /// uid 是否已被占用。
    pub fn uid_taken(&self, uid: &str) -> bool {
        self.users.read().contains_key(uid)
    }

    /// 用户总数。
    pub fn count(&self) -> usize {
        self.users.read().len()
    }

    /// 全部用户（测试/管理用）。
    pub fn all_users(&self) -> Vec<User> {
        self.users.read().values().cloned().collect()
    }

    // ── 写入 ────────────────────────────────────────────────────────────────

    /// 注册新用户（D1: /register 或 /link 成功时创建 active）。
    /// 校验 uid（格式+保留字+唯一）与 email（格式+唯一），全部通过才写入。
    pub fn register(
        &self,
        email: &str,
        uid: &str,
        nickname: Option<&str>,
    ) -> Result<User, RegisterError> {
        let uid = uid.trim();
        validate_uid(uid)?;
        let email = validate_email(email)?;
        if let Some(nick) = nickname {
            validate_nickname(nick)?;
        }
        let mut users = self.users.write();
        let mut emails = self.email_index.write();
        if users.contains_key(uid) {
            return Err(RegisterError::UidTaken(uid.to_string()));
        }
        if emails.contains_key(&email) {
            return Err(RegisterError::EmailTaken(email));
        }
        let user = User {
            uid: uid.to_string(),
            email: Some(email.clone()),
            nickname: nickname.map(|n| n.to_string()),
            active: true,
            created_ms: now_ms(),
        };
        users.insert(uid.to_string(), user.clone());
        emails.insert(email, uid.to_string());
        drop(users);
        drop(emails);
        self.save();
        Ok(user)
    }

    /// 更换邮箱（RFC D4: email 唯一 + 可更换，旧值释放）。
    /// 传空字符串视为仅查询校验（无实际更新）——调用方负责文案。
    pub fn set_email(&self, uid: &str, new_email: &str) -> Result<(), RegisterError> {
        let new_email = validate_email(new_email)?;
        let mut users = self.users.write();
        let mut emails = self.email_index.write();
        let user = users
            .get_mut(uid)
            .ok_or_else(|| RegisterError::NoSuchUser(uid.to_string()))?;
        if let Some(old) = &user.email {
            if *old == new_email {
                return Ok(()); // 无变化
            }
            emails.remove(old);
        }
        if emails.contains_key(&new_email) {
            // 释放旧值失败——回滚。
            if let Some(old) = &user.email {
                emails.insert(old.clone(), uid.to_string());
            }
            return Err(RegisterError::EmailTaken(new_email));
        }
        emails.insert(new_email.clone(), uid.to_string());
        user.email = Some(new_email);
        drop(users);
        drop(emails);
        self.save();
        Ok(())
    }

    /// 设置昵称（可重复；不允许含 `/`，RFC §2.2 结构判定依赖此约束）。
    pub fn set_nickname(&self, uid: &str, nick: &str) -> Result<(), RegisterError> {
        let nick = validate_nickname(nick)?;
        let mut users = self.users.write();
        let user = users
            .get_mut(uid)
            .ok_or_else(|| RegisterError::NoSuchUser(uid.to_string()))?;
        user.nickname = Some(nick);
        drop(users);
        self.save();
        Ok(())
    }

    /// 确保 root 用户存在（迁移时创建；幂等）。
    pub fn ensure_root(&self) {
        let mut users = self.users.write();
        if !users.contains_key(ROOT_UID) {
            users.insert(
                ROOT_UID.to_string(),
                User {
                    uid: ROOT_UID.to_string(),
                    email: None,
                    nickname: None,
                    active: true,
                    created_ms: now_ms(),
                },
            );
            drop(users);
            self.save();
        }
    }

    // ── 一次性迁移（RFC §2.2 / §2.3: 存量归 root）─────────────────────────

    /// 把存量 identity 全部归入 root 用户（P4 迁移）。
    ///
    /// 幂等：root 已存在即视为已迁移（no-op）。迁移动作：
    /// 1. `known_users` 登记簿中全部存量 routing_key → 绑定 root；
    /// 2. `user_resolver.json` 中已有绑定（P3 /link 的短 id 值）同样归 root；
    /// 3. contacts / user_mailbox 中非 routing_key 形态的键（P3 折叠短 id）
    ///    统一重指 root（P4 后折叠值一律 user.id，避免新旧混用导致关系分裂）；
    /// 4. 创建 root User 实体。
    ///
    /// 调用时机：daemon 启动、`known_users.migrate_legacy` 之后。
    pub fn migrate_legacy_to_root(&self, known: &KnownUsersRegistry, resolver: &UserResolver) {
        if self.find_by_uid(ROOT_UID).is_some() {
            return; // 已迁移
        }
        let root_id = self.user_id_of(ROOT_UID);
        // 1. 登记簿存量 rk 全绑 root（migrate_identity 顺带把 rk 名下
        //    contacts/mailbox 并入 root）。
        for rk in known.rk_keys() {
            resolver.set(&rk, &root_id);
            known.migrate_identity(&rk, &root_id);
        }
        // 2. resolver 中已有绑定（含 users map 之外的 rk）归 root。
        for rk in resolver.all_routing_keys() {
            resolver.set(&rk, &root_id);
            known.migrate_identity(&rk, &root_id);
        }
        // 3. 非 rk 形态的折叠键（P3 短 id）重指 root。
        known.rekey_legacy_to(&root_id);
        // 4. 创建 root 实体。
        self.ensure_root();
        info!(
            users = known.rk_keys().len(),
            "user_registry: migrated legacy identities to myclaw/u/root"
        );
    }

    // ── 持久化 ──────────────────────────────────────────────────────────────

    fn load_from_disk(&self) {
        if self.data_path.as_os_str().is_empty() {
            return;
        }
        let contents = match std::fs::read_to_string(&self.data_path) {
            Ok(c) => c,
            Err(_) => return, // 首次运行，无文件
        };
        match serde_json::from_str::<PersistedUsers>(&contents) {
            Ok(file) => {
                let mut users = self.users.write();
                let mut emails = self.email_index.write();
                for (uid, user) in file.users {
                    if let Some(email) = &user.email {
                        emails.insert(email.clone(), uid.clone());
                    }
                    users.insert(uid, user);
                }
                info!(
                    path = %self.data_path.display(),
                    count = users.len(),
                    "user_registry: loaded from disk"
                );
            }
            Err(e) => {
                warn!(
                    path = %self.data_path.display(),
                    err = %e,
                    "user_registry: failed to parse, starting empty"
                );
            }
        }
    }

    fn save(&self) {
        if self.data_path.as_os_str().is_empty() {
            return;
        }
        let body = match serde_json::to_vec_pretty(&PersistedUsers {
            version: 1,
            users: self.users.read().clone(),
        }) {
            Ok(b) => b,
            Err(e) => {
                warn!(err = %e, "user_registry: serialization failed");
                return;
            }
        };
        if let Some(parent) = self.data_path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                warn!(
                    path = %parent.display(),
                    err = %e,
                    "user_registry: failed to create data dir"
                );
                return;
            }
        }
        if let Err(e) = std::fs::write(&self.data_path, body) {
            warn!(
                path = %self.data_path.display(),
                err = %e,
                "user_registry: failed to persist users"
            );
        }
    }
}

// ── 校验（独立函数，便于测试与复用）────────────────────────────────────────

/// 邮箱小写归一化（trim + lowercase）。
pub fn normalize_email(email: &str) -> String {
    email.trim().to_lowercase()
}

/// 校验 uid：`[a-z0-9_]{3,32}`、保留字不可用。
pub fn validate_uid(uid: &str) -> Result<(), RegisterError> {
    if uid.len() < UID_MIN || uid.len() > UID_MAX {
        return Err(RegisterError::InvalidUid(format!(
            "uid 长度须为 {UID_MIN}–{UID_MAX} 个字符（当前 {} 个）",
            uid.len()
        )));
    }
    if !uid
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        return Err(RegisterError::InvalidUid(
            "uid 只能包含小写字母、数字和下划线（[a-z0-9_]+）".to_string(),
        ));
    }
    if RESERVED_UIDS.contains(&uid) {
        return Err(RegisterError::ReservedUid(uid.to_string()));
    }
    Ok(())
}

/// 校验邮箱：非空、含单个 `@`、长度 ≤ 254；返回归一化结果。
pub fn validate_email(email: &str) -> Result<String, RegisterError> {
    let email = normalize_email(email);
    if email.is_empty() || !email.contains('@') || email.len() > 254 {
        return Err(RegisterError::InvalidEmail(
            "邮箱格式不正确（应为 user@example.com）".to_string(),
        ));
    }
    let (local, domain) = email.split_once('@').expect("contains @ checked above");
    if local.is_empty() || domain.is_empty() || domain.contains('@') {
        return Err(RegisterError::InvalidEmail(
            "邮箱格式不正确（应为 user@example.com）".to_string(),
        ));
    }
    Ok(email)
}

/// 校验昵称：trim 后非空、≤ NICK_MAX、不允许含 `/`（RFC §2.2 结构判定）。
pub fn validate_nickname(nick: &str) -> Result<String, RegisterError> {
    let nick = nick.trim();
    if nick.is_empty() {
        return Err(RegisterError::InvalidNickname(
            "昵称不能为空".to_string(),
        ));
    }
    if nick.len() > NICK_MAX {
        return Err(RegisterError::InvalidNickname(format!(
            "昵称最长 {NICK_MAX} 个字符"
        )));
    }
    if nick.contains('/') {
        return Err(RegisterError::InvalidNickname(
            "昵称不能包含 /（它用于区分 u/uid 形式的 id）".to_string(),
        ));
    }
    Ok(nick.to_string())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
