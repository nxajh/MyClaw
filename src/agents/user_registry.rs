//! UserRegistry — P4 用户实体层（uid / username / email），与渠道登记簿分离。
//!
//! RFC §2.2（身份模型重构后）: 每个用户是独立的 `User` 实体。`uid` 是系统
//! 分配的不可变内部键（`<ns>/u/<uuidv7>`，不可读、不可选、先到先得）；
//! `username` 是唯一可改的对外标识（`[a-z0-9_]{3,32}`、保留字不可用、
//! 先到先得），`@username` 全局解析与显示层都走它。`email` 唯一且可更换。
//! 渠道侧 routing_key 到用户的映射仍由 [`UserResolver`] 承担（值统一为
//! user.id，如 `myclaw/u/<uuidv7>`）；本注册表只存"人"的实体属性，不感知渠道。
//!
//! 持久化在 `{data_dir}/users.json`（version 2：`uid`=uuidv7 +
//! `username` 字段），每次变更写盘（数据量小，与 `user_resolver.json` 同一
//! 策略）。读入兼容 version 1 旧文件（无 `username` 字段 → 空串；`uid` 为旧
//! 语义句柄），语义错位由迁移（task: migrate）统一修正。命名空间默认
//! `myclaw`（RFC: `[system] namespace` 配置项，后续接入）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::agents::{KnownUsersRegistry, UserResolver};
use crate::ids::{Fqid, TYPE_USER};

// ── 常量与规则 ───────────────────────────────────────────────────────────────

/// 保留字（username 不可用）：root 及系统级保留（RFC §2.2）。
pub const RESERVED_USERNAMES: &[&str] = &["root", "admin", "system", "bot", "help", "register"];
/// username 长度下限。
pub const USERNAME_MIN: usize = 3;
/// username 长度上限。
pub const USERNAME_MAX: usize = 32;
/// 默认命名空间（RFC: [system] namespace，配置项后续接入）。
pub const DEFAULT_NAMESPACE: &str = "myclaw";
/// root 的固定 username（语义名保留字；uid 为系统分配 uuidv7）。
pub const ROOT_USERNAME: &str = "root";

/// 由 uid 构造完整 user.id（三段式 FQID：`<namespace>/u/<uid>`）。
pub fn user_id(namespace: &str, uid: &str) -> String {
    format!("{namespace}/u/{uid}")
}

// ── 用户实体 ─────────────────────────────────────────────────────────────────

/// 一个用户（P4 用户实体层）。`uid` 是不可变系统内部键（uuidv7）；
/// `username` 是唯一可改的对外标识（`[a-z0-9_]{3,32}`，先到先得）。
///
/// 旧数据兼容：version 1 的 users.json 没有 `username` 字段（解析为 ""，
/// 显示层/解析层过滤空串；语义修正属迁移职责）；`uid` 缺失时同样兜底。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    /// 系统分配的内部键（`<ns>/u/<uuidv7>`，不可变、不可读、不可选）。
    #[serde(default)]
    pub uid: String,
    /// 唯一对外标识（`[a-z0-9_]{3,32}`，可改、先到先得；旧数据为空串）。
    #[serde(default)]
    pub username: String,
    /// 唯一邮箱（小写归一化；未验证状态下也先占位）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
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
    /// username 格式非法（字符/长度）。
    InvalidUsername(String),
    /// username 为保留字。
    ReservedUsername(String),
    /// username 已被占用（先到先得）。
    UsernameTaken(String),
    /// email 格式非法。
    InvalidEmail(String),
    /// email 已被占用（唯一）。
    EmailTaken(String),
    /// 用户不存在（更新类操作）。
    NoSuchUser(String),
}

// ── 注册表 ───────────────────────────────────────────────────────────────────

/// 用户实体注册表：`uid → User` + `email → uid` + `username → uid` 索引，
/// 持久化 `users.json`。
#[derive(Debug)]
pub struct UserRegistry {
    users: RwLock<HashMap<String, User>>,
    /// email（小写）→ uid 唯一索引。
    email_index: RwLock<HashMap<String, String>>,
    /// username（小写）→ uid 唯一索引（派生，不落盘，加载时重建）。
    username_index: RwLock<HashMap<String, String>>,
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
            username_index: RwLock::new(HashMap::new()),
            data_path: PathBuf::new(),
            namespace: DEFAULT_NAMESPACE.to_string(),
        }
    }

    /// 持久化注册表（`{data_dir}/users.json`），默认命名空间 `myclaw`。
    pub fn persistent(data_dir: &Path) -> Self {
        Self::with_namespace(data_dir, DEFAULT_NAMESPACE)
    }

    /// 持久化注册表 + 自定义命名空间（RFC: [system] namespace 配置项接入点）。
    pub fn with_namespace(data_dir: &Path, namespace: &str) -> Self {
        let data_path = data_dir.join("users.json");
        let reg = Self {
            users: RwLock::new(HashMap::new()),
            email_index: RwLock::new(HashMap::new()),
            username_index: RwLock::new(HashMap::new()),
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

    /// 从 user.id 提取 uid 内部键（非本实例 id 返回 None）。
    pub fn uid_of<'a>(&self, user_id_str: &'a str) -> Option<&'a str> {
        user_id_str.strip_prefix(&format!("{}/u/", self.namespace))
    }

    /// 用户可见显示形态（RFC §2.2 显示层）：
    /// 有 username → `@username`；非本实例 id 或查不到（含旧数据空串）→ 原样返回。
    pub fn display(&self, user_id_str: &str) -> String {
        match self.username_of(user_id_str) {
            Some(username) => format!("@{username}"),
            None => user_id_str.to_string(),
        }
    }

    /// 实时 username（RFC §2.2: 标识不落快照，显示/比对一律实时取）。
    /// 过滤空串（旧数据未设 username 视为无）。
    pub fn username_of(&self, user_id_str: &str) -> Option<String> {
        let uid = self.uid_of(user_id_str)?;
        let users = self.users.read();
        let user = users.get(uid)?;
        (!user.username.is_empty()).then(|| user.username.clone())
    }

    // ── 查询 ────────────────────────────────────────────────────────────────

    /// 按 uid（内部键，不带 `u/` 前缀）查询。
    pub fn find_by_uid(&self, uid: &str) -> Option<User> {
        self.users.read().get(uid).cloned()
    }

    /// 按 username 查询（大小写不敏感——内部小写归一化）。
    pub fn find_by_username(&self, username: &str) -> Option<User> {
        let username = username.trim().to_lowercase();
        let uid = self.username_index.read().get(&username)?.clone();
        self.users.read().get(&uid).cloned()
    }

    /// 按邮箱查询（大小写不敏感——内部小写归一化）。
    pub fn find_by_email(&self, email: &str) -> Option<User> {
        let email = normalize_email(email);
        let uid = self.email_index.read().get(&email)?.clone();
        self.users.read().get(&uid).cloned()
    }

    /// username 是否已被占用。
    pub fn username_taken(&self, username: &str) -> bool {
        let username = username.trim().to_lowercase();
        self.username_index.read().contains_key(&username)
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
    /// uid 由系统分配（uuidv7，不可选）；校验 username（格式+保留字+唯一）
    /// 与 email（格式+唯一），全部通过才写入。
    pub fn register(&self, email: &str, username: &str) -> Result<User, RegisterError> {
        let username = validate_username(username)?;
        let email = validate_email(email)?;
        let uid = Fqid::new(&self.namespace, TYPE_USER).to_string();
        let mut users = self.users.write();
        let mut emails = self.email_index.write();
        let mut usernames = self.username_index.write();
        if usernames.contains_key(&username) {
            return Err(RegisterError::UsernameTaken(username));
        }
        if emails.contains_key(&email) {
            return Err(RegisterError::EmailTaken(email));
        }
        let user = User {
            uid: uid.clone(),
            username: username.clone(),
            email: Some(email.clone()),
            active: true,
            created_ms: now_ms(),
        };
        users.insert(uid.clone(), user.clone());
        emails.insert(email, uid.clone());
        usernames.insert(username, uid);
        drop(users);
        drop(emails);
        drop(usernames);
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

    /// 设置 username（唯一、可改；旧值释放，`@username` 全局解析随之生效）。
    pub fn set_username(&self, uid: &str, new_username: &str) -> Result<(), RegisterError> {
        let new_username = validate_username(new_username)?;
        let mut users = self.users.write();
        let mut usernames = self.username_index.write();
        let user = users
            .get_mut(uid)
            .ok_or_else(|| RegisterError::NoSuchUser(uid.to_string()))?;
        if user.username == new_username {
            return Ok(()); // 无变化
        }
        if let Some(owner) = usernames.get(&new_username) {
            if owner != uid {
                return Err(RegisterError::UsernameTaken(new_username));
            }
        }
        if !user.username.is_empty() {
            usernames.remove(&user.username);
        }
        usernames.insert(new_username.clone(), uid.to_string());
        user.username = new_username;
        drop(users);
        drop(usernames);
        self.save();
        Ok(())
    }

    /// 确保 root 用户存在（迁移时创建；幂等）。root 的 `username` 固定为
    /// 保留字 `root`，uid 为系统分配 uuidv7。
    pub fn ensure_root(&self) {
        let mut users = self.users.write();
        let mut usernames = self.username_index.write();
        if usernames.contains_key(ROOT_USERNAME) {
            return;
        }
        let uid = Fqid::new(&self.namespace, TYPE_USER).to_string();
        let user = User {
            uid: uid.clone(),
            username: ROOT_USERNAME.to_string(),
            email: None,
            active: true,
            created_ms: now_ms(),
        };
        users.insert(uid.clone(), user.clone());
        usernames.insert(ROOT_USERNAME.to_string(), uid);
        drop(users);
        drop(usernames);
        self.save();
    }

    // ── 一次性迁移（RFC §2.2 / §2.3: 存量归 root）─────────────────────────

    /// 把存量 identity 全部归入 root 用户（P4 迁移）。
    ///
    /// 幂等：root 已存在即视为已迁移（no-op）。迁移动作：
    /// 1. `known_users` 登记簿中全部存量 routing_key → 绑定 root；
    /// 2. `user_resolver.json` 中已有绑定（P3 /link 的短 id 值）同样归 root；
    /// 3. contacts / user_mailbox 中非 routing_key 形态的键（P3 折叠短 id）
    ///    统一重指 root（P4 后折叠值一律 user.id，避免新旧混用导致关系分裂）；
    /// 4. 创建 root User 实体（username=`root`，uid=uuidv7）。
    ///
    /// 调用时机：daemon 启动、`known_users.migrate_legacy` 之后。
    pub fn migrate_legacy_to_root(&self, known: &KnownUsersRegistry, resolver: &UserResolver) {
        if self.find_by_username(ROOT_USERNAME).is_some() {
            return; // 已迁移
        }
        // 先创建 root 实体，拿到其 uuidv7 user.id 作为折叠目标。
        self.ensure_root();
        let root_id = self
            .find_by_username(ROOT_USERNAME)
            .expect("root created above")
            .user_id(&self.namespace);
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
        info!(
            users = known.rk_keys().len(),
            "user_registry: migrated legacy identities to {root_id}"
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
                let mut usernames = self.username_index.write();
                for (uid, user) in file.users {
                    if let Some(email) = &user.email {
                        emails.insert(email.clone(), uid.clone());
                    }
                    if !user.username.is_empty() {
                        usernames.insert(user.username.clone(), uid.clone());
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
            version: 2,
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

/// 校验 username：`[a-z0-9_]{3,32}`（小写归一化）、保留字不可用；返回归一化结果。
pub fn validate_username(username: &str) -> Result<String, RegisterError> {
    let username = username.trim().to_lowercase();
    if username.len() < USERNAME_MIN || username.len() > USERNAME_MAX {
        return Err(RegisterError::InvalidUsername(format!(
            "username 长度须为 {USERNAME_MIN}–{USERNAME_MAX} 个字符（当前 {} 个）",
            username.len()
        )));
    }
    if !username
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        return Err(RegisterError::InvalidUsername(
            "username 只能包含小写字母、数字和下划线（[a-z0-9_]+）".to_string(),
        ));
    }
    if RESERVED_USERNAMES.contains(&username.as_str()) {
        return Err(RegisterError::ReservedUsername(username));
    }
    Ok(username)
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

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
