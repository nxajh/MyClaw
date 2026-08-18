//! UserRegistry — P4 用户实体层（uid / username / email），与渠道登记簿分离。
//!
//! RFC §2.2（身份模型重构后）: 每个用户是独立的 `User` 实体。`uid` 是系统
//! 分配的不可变内部键（`<ns>/u/<uuidv7>`，不可读、不可选、先到先得）；
//! `username` 是唯一可改的对外标识（`[a-z0-9_]{3,32}`、保留字不可用、
//! 先到先得），`@username` 全局解析与显示层都走它。`email` 唯一且可更换。
//! 渠道侧 routing_key 到用户的映射仍由 [`UserResolver`] 承担（值统一为
//! user.id，如 `myclaw/u/<uuidv7>`）；本注册表只存"人"的实体属性，不感知渠道。
//!
//! 持久化（P1-B1 目录化）：每用户一个 `{data_dir}/users/{uuid}/meta.json`
//! （内容 = [`User`] 完整序列化，`uid` 为完整 FQID `<ns>/u/<uuid>`），
//! 变更只写该用户的 meta.json，不再全量重写单文件。加载优先扫目录；目录化
//! 数据不存在时兜底读旧 `{data_dir}/users.json`（version 2；兼容 version 1
//! 旧文件——无 `username` 字段 → 空串；`uid` 为旧语义句柄），语义错位由迁移
//! （task: migrate）统一修正。命名空间默认 `myclaw`（RFC: `[system]
//! namespace` 配置项，后续接入）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::agents::{KnownUsersRegistry, UserResolver};
use crate::ids::{bare_dir_name, Fqid, TYPE_USER};

// ── 常量与规则 ───────────────────────────────────────────────────────────────

/// 目录化存储的用户实体根目录名：`{data_dir}/users/`。
pub const USERS_DIR: &str = "users";
/// 每用户实体的元数据文件名：`{data_dir}/users/{uuid}/meta.json`。
pub const USER_META_FILE: &str = "meta.json";
/// 旧版单文件存储文件名（兼容兜底读，不再写）。
pub const LEGACY_USERS_FILE: &str = "users.json";

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
///
/// 幂等：uid 已以 `<namespace>/u/` 开头（本身已是完整 FQID——注册/迁移后的
/// 规范形态）→ 原样返回，避免双重前缀污染（旧 bug：对 FQID uid 二次加前缀
/// 产生 `ns/u/ns/u/<uuid>`，曾污染 user_resolver.json）。
pub fn user_id(namespace: &str, uid: &str) -> String {
    if uid.starts_with(&format!("{namespace}/u/")) {
        uid.to_string()
    } else {
        format!("{namespace}/u/{uid}")
    }
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
/// 持久化 `{data_dir}/users/{uuid}/meta.json`（P1-B1 目录化；旧
/// `users.json` 仅兜底读）。
#[derive(Debug)]
pub struct UserRegistry {
    users: RwLock<HashMap<String, User>>,
    /// email（小写）→ uid 唯一索引。
    email_index: RwLock<HashMap<String, String>>,
    /// username（小写）→ uid 唯一索引（派生，不落盘，加载时重建）。
    username_index: RwLock<HashMap<String, String>>,
    /// 持久化根（`{data_dir}`；空 = 内存态，不落盘）。
    data_dir: PathBuf,
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
            data_dir: PathBuf::new(),
            namespace: DEFAULT_NAMESPACE.to_string(),
        }
    }

    /// 持久化注册表（`{data_dir}/users/`，P1-B1 目录化），默认命名空间
    /// `myclaw`。
    pub fn persistent(data_dir: &Path) -> Self {
        Self::with_namespace(data_dir, DEFAULT_NAMESPACE)
    }

    /// 持久化注册表 + 自定义命名空间（RFC: [system] namespace 配置接入点）。
    pub fn with_namespace(data_dir: &Path, namespace: &str) -> Self {
        let reg = Self {
            users: RwLock::new(HashMap::new()),
            email_index: RwLock::new(HashMap::new()),
            username_index: RwLock::new(HashMap::new()),
            data_dir: data_dir.to_path_buf(),
            namespace: namespace.to_string(),
        };
        reg.load_from_disk();
        reg
    }

    /// 用户实体根目录（`{data_dir}/users/`）。
    fn users_root(&self) -> PathBuf {
        self.data_dir.join(USERS_DIR)
    }

    /// 单个用户的目录：`{data_dir}/users/{uuid}/`。
    ///
    /// 目录名取 uid（完整 FQID `<ns>/u/<uuid>`）的裸 uuid 段；uid 不是
    /// 合法 FQID（旧语义句柄等）时退化为 `dir_name` 转义，保证路径安全。
    fn user_dir(&self, uid: &str) -> PathBuf {
        self.users_root().join(bare_dir_name(uid))
    }

    /// 单个用户的 meta.json 路径。
    fn user_meta_path(&self, uid: &str) -> PathBuf {
        self.user_dir(uid).join(USER_META_FILE)
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

    /// 从 user.id 提取规范 uid 内部键（= 完整 FQID `<ns>/u/<uuid>`，users.json
    /// 的 map key 即此形态；非本实例 id 返回 None）。
    ///
    /// 兼治双重前缀输入（`ns/u/ns/u/<uuid>`，旧 user_id() bug 污染值）→ 剥到
    /// 单层 FQID。旧语义（返回裸 uuid）与 map key 不匹配，导致 username_of /
    /// set_email / set_username / info 展示全部落空——本实现返回 FQID 后
    /// `users.get(uid)` 直接命中。
    pub fn uid_of<'a>(&self, user_id_str: &'a str) -> Option<&'a str> {
        let prefix = format!("{}/u/", self.namespace);
        let rest = user_id_str.strip_prefix(&prefix)?;
        if rest.starts_with(&prefix) {
            Some(rest) // 双重前缀：rest 已是规范 FQID
        } else {
            Some(user_id_str) // 规范 FQID → 原样
        }
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

    /// 按 uid 查询：完整 FQID、裸 uuid 双形态可查（兼容 `parse_target` 的
    /// `u/<uuid>` 剥离形态与旧数据键）。
    pub fn find_by_uid(&self, uid: &str) -> Option<User> {
        let users = self.users.read();
        if let Some(u) = users.get(uid) {
            return Some(u.clone());
        }
        let prefix = format!("{}/u/", self.namespace);
        let bare = uid.strip_prefix(&prefix).unwrap_or(uid);
        if let Some(u) = users.get(bare) {
            return Some(u.clone());
        }
        // 裸 uuid 输入 → 按 FQID 后缀匹配（规范键 = `<ns>/u/<uuid>`）。
        let suffix = format!("/{bare}");
        users
            .iter()
            .find(|(k, _)| k.ends_with(&suffix))
            .map(|(_, u)| u.clone())
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
        usernames.insert(username, uid.clone());
        drop(users);
        drop(emails);
        drop(usernames);
        self.save_user(&uid);
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
        self.save_user(uid);
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
        self.save_user(uid);
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
        usernames.insert(ROOT_USERNAME.to_string(), uid.clone());
        drop(users);
        drop(usernames);
        self.save_user(&uid);
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

    /// 加载：优先扫 `{data_dir}/users/{uuid}/meta.json` 逐个加载；目录化数据
    /// 不存在（无 `users/` 目录或目录为空）时兜底读旧 `users.json`。
    /// 存量 users.json → 目录的拆分由迁移脚本负责，代码不做自动迁移。
    fn load_from_disk(&self) {
        if self.data_dir.as_os_str().is_empty() {
            return;
        }
        let count = self.load_from_users_dir();
        if count > 0 {
            info!(count, "user_registry: loaded from users/ directory");
        } else {
            // 目录化数据不存在（首次运行或迁移脚本未跑）→ 兼容兜底。
            self.load_from_legacy_file();
        }
    }

    /// 扫 `{data_dir}/users/` 逐个加载 `meta.json`。单个文件损坏只跳过
    /// 该用户（warn），不影响其余。返回成功加载的用户数。
    fn load_from_users_dir(&self) -> usize {
        let root = self.users_root();
        let entries = match std::fs::read_dir(&root) {
            Ok(e) => e,
            Err(_) => return 0, // 无 users/ 目录 → 由调用方走 users.json 兜底
        };
        let mut users = self.users.write();
        let mut emails = self.email_index.write();
        let mut usernames = self.username_index.write();
        let mut count = 0usize;
        for entry in entries.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            let meta = entry.path().join(USER_META_FILE);
            let contents = match std::fs::read_to_string(&meta) {
                Ok(c) => c,
                Err(_) => continue, // 空目录 / 非 meta 目录（嵌套布局等）
            };
            match serde_json::from_str::<User>(&contents) {
                Ok(user) => {
                    // map key 一律用 user.uid（完整 FQID；文件内容为准，
                    // 目录名仅是物理布局）。
                    let uid = user.uid.clone();
                    if let Some(email) = &user.email {
                        emails.insert(email.clone(), uid.clone());
                    }
                    if !user.username.is_empty() {
                        usernames.insert(user.username.clone(), uid.clone());
                    }
                    users.insert(uid, user);
                    count += 1;
                }
                Err(e) => {
                    warn!(
                        path = %meta.display(),
                        err = %e,
                        "user_registry: skipping unparsable user meta"
                    );
                }
            }
        }
        count
    }

    /// 读旧 `{data_dir}/users.json`（version 1/2 单文件）。仅在目录化数据
    /// 不存在时调用（兼容兜底）。
    fn load_from_legacy_file(&self) {
        let path = self.data_dir.join(LEGACY_USERS_FILE);
        let contents = match std::fs::read_to_string(&path) {
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
                    path = %path.display(),
                    count = users.len(),
                    "user_registry: loaded from legacy users.json"
                );
            }
            Err(e) => {
                warn!(
                    path = %path.display(),
                    err = %e,
                    "user_registry: failed to parse, starting empty"
                );
            }
        }
    }

    /// 写单个用户的 meta.json（`users/{uuid}/meta.json`，先建目录）。
    /// P1-B1：变更只落对应用户文件，不再全量重写 users.json。
    fn save_user(&self, uid: &str) {
        if self.data_dir.as_os_str().is_empty() {
            return;
        }
        let Some(user) = self.users.read().get(uid).cloned() else {
            return;
        };
        let path = self.user_meta_path(uid);
        if let Err(e) = std::fs::create_dir_all(path.parent().unwrap_or(&self.users_root())) {
            warn!(
                path = %path.display(),
                err = %e,
                "user_registry: failed to create user dir"
            );
            return;
        }
        match serde_json::to_vec_pretty(&user) {
            Ok(body) => {
                if let Err(e) = std::fs::write(&path, body) {
                    warn!(
                        path = %path.display(),
                        err = %e,
                        "user_registry: failed to persist user meta"
                    );
                }
            }
            Err(e) => warn!(err = %e, "user_registry: serialization failed"),
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

// ── 测试 ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn reg() -> UserRegistry {
        UserRegistry::in_memory()
    }

    /// user_id() 幂等：FQID uid 不再二次加前缀（旧 bug 产生双重前缀污染）。
    #[test]
    fn user_id_is_idempotent_for_fqid() {
        let bare = "019fe342-6a03-7561-86de-0c2327a8c3de";
        assert_eq!(
            user_id("myclaw", bare),
            "myclaw/u/019fe342-6a03-7561-86de-0c2327a8c3de"
        );
        let fqid = "myclaw/u/019fe342-6a03-7561-86de-0c2327a8c3de";
        assert_eq!(user_id("myclaw", fqid), fqid);
        // 双重前缀输入不再继续叠加。
        let double = "myclaw/u/myclaw/u/019fe342-6a03-7561-86de-0c2327a8c3de";
        assert_eq!(user_id("myclaw", double), double);
    }

    /// uid_of() 返回规范 FQID（users.json map key 形态）；双重前缀剥到单层。
    #[test]
    fn uid_of_returns_canonical_fqid_and_strips_double_prefix() {
        let r = reg();
        let u = r.register("alice@example.com", "alice").unwrap();
        let fqid = u.uid.clone();
        assert!(fqid.starts_with("myclaw/u/"));
        assert_eq!(r.uid_of(&fqid), Some(fqid.as_str()));
        let double = format!("myclaw/u/{fqid}");
        assert_eq!(r.uid_of(&double), Some(fqid.as_str()));
        assert_eq!(r.uid_of("brand/u/019fe342-6a03-7561-86de-0c2327a8c3de"), None);
    }

    /// username_of() 用 FQID 直接命中 map（旧 bug：裸 uuid 落空）。
    #[test]
    fn username_of_hits_map_with_fqid() {
        let r = reg();
        let u = r.register("alice@example.com", "alice").unwrap();
        assert_eq!(r.username_of(&u.uid), Some("alice".to_string()));
        assert_eq!(r.username_of("myclaw/u/019fe342-6a03-7561-86de-0c2327a8c3de"), None);
    }

    /// find_by_uid() 双形态可查：FQID / 裸 uuid / `u/<uuid>`（parse_target 形态）。
    #[test]
    fn find_by_uid_accepts_fqid_bare_and_u_prefix() {
        let r = reg();
        let u = r.register("alice@example.com", "alice").unwrap();
        let fqid = u.uid.clone();
        let bare = fqid.strip_prefix("myclaw/u/").unwrap();
        assert_eq!(r.find_by_uid(&fqid).unwrap().uid, fqid);
        assert_eq!(r.find_by_uid(bare).unwrap().uid, fqid);
        assert_eq!(r.find_by_uid(&format!("u/{bare}")).unwrap().uid, fqid);
        assert!(r.find_by_uid("deadbeef-0000-0000-0000-000000000000").is_none());
    }

    /// set_email / set_username 用 FQID 命中（current_uid() 经 uid_of 返回规范
    /// FQID 后的生产路径）。
    #[test]
    fn set_email_and_username_work_with_fqid() {
        let r = reg();
        let u = r.register("alice@example.com", "alice").unwrap();
        let fqid = u.uid.clone();
        r.set_email(&fqid, "new@example.com").unwrap();
        assert_eq!(
            r.find_by_uid(&fqid).unwrap().email.as_deref(),
            Some("new@example.com")
        );
        r.set_username(&fqid, "bob").unwrap();
        assert_eq!(r.username_of(&fqid), Some("bob".to_string()));
        // username 变更后 @username 解析生效。
        assert_eq!(r.find_by_username("bob").unwrap().uid, fqid);
    }

    // ── P1-B1 目录化持久化 ────────────────────────────────────────────────

    /// 优先扫 `users/{uuid}/meta.json`：目录化数据存在时不再读 users.json。
    #[test]
    fn directory_layout_takes_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();

        // 目录化：一个用户。
        let reg = UserRegistry::with_namespace(data, DEFAULT_NAMESPACE);
        let u = reg.register("alice@example.com", "alice").unwrap();
        drop(reg);

        // 同时放一个内容不同的旧 users.json（若被读到，用户名会是 eve）。
        let legacy = serde_json::json!({
            "version": 2,
            "users": {
                "myclaw/u/00000000-0000-7000-8000-000000000001": {
                    "uid": "myclaw/u/00000000-0000-7000-8000-000000000001",
                    "username": "eve",
                    "email": "eve@example.com",
                    "active": true,
                    "created_ms": 1u64
                }
            }
        });
        std::fs::write(
            data.join("users.json"),
            serde_json::to_string_pretty(&legacy).unwrap(),
        )
        .unwrap();

        // 重开：目录数据生效（alice 在、eve 不在）。
        let re = UserRegistry::with_namespace(data, DEFAULT_NAMESPACE);
        assert!(re.find_by_username("alice").is_some());
        assert!(re.find_by_username("eve").is_none());
        assert_eq!(re.count(), 1);
        assert_eq!(re.find_by_uid(&u.uid).unwrap().username, "alice");

        // meta.json 位于 users/{bare-uuid}/meta.json。
        let bare = u.uid.strip_prefix("myclaw/u/").unwrap();
        let meta = data.join("users").join(bare).join("meta.json");
        assert!(meta.is_file());
    }

    /// 目录化数据不存在时兜底读旧 users.json（v2 单文件）。
    #[test]
    fn legacy_users_json_fallback_load() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        let legacy = serde_json::json!({
            "version": 2,
            "users": {
                "myclaw/u/00000000-0000-7000-8000-000000000002": {
                    "uid": "myclaw/u/00000000-0000-7000-8000-000000000002",
                    "username": "bob",
                    "email": "bob@example.com",
                    "active": true,
                    "created_ms": 1u64
                }
            }
        });
        std::fs::write(
            data.join("users.json"),
            serde_json::to_string_pretty(&legacy).unwrap(),
        )
        .unwrap();

        let reg = UserRegistry::with_namespace(data, DEFAULT_NAMESPACE);
        assert_eq!(reg.count(), 1);
        assert!(reg.find_by_username("bob").is_some());
        assert!(reg.find_by_email("BOB@example.com").is_some());

        // 兜底加载后的写操作落目录化 meta.json（不再改写 users.json）。
        let u = reg.find_by_username("bob").unwrap();
        reg.set_email(&u.uid, "new@example.com").unwrap();
        let bare = u.uid.strip_prefix("myclaw/u/").unwrap();
        let meta = data.join("users").join(bare).join("meta.json");
        assert!(meta.is_file());
        let on_disk: User =
            serde_json::from_str(&std::fs::read_to_string(&meta).unwrap()).unwrap();
        assert_eq!(on_disk.email.as_deref(), Some("new@example.com"));

        // users.json 保持原样（不再被全量重写）。
        let legacy_raw = std::fs::read_to_string(data.join("users.json")).unwrap();
        let legacy_back: serde_json::Value = serde_json::from_str(&legacy_raw).unwrap();
        assert_eq!(
            legacy_back["users"]["myclaw/u/00000000-0000-7000-8000-000000000002"]["email"],
            "bob@example.com"
        );
    }

    /// 写操作只落对应 meta.json：register/ensure_root 各自只写自己的文件。
    #[test]
    fn writes_are_localized_per_user_meta() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        let reg = UserRegistry::with_namespace(data, DEFAULT_NAMESPACE);

        reg.ensure_root();
        let root = reg.find_by_username("root").unwrap();
        let a = reg.register("a@example.com", "alice").unwrap();
        let b = reg.register("b@example.com", "bob").unwrap();

        // 每个用户都有自己的 meta.json。
        for (uid, username) in [(root.uid.as_str(), "root"), (&a.uid, "alice"), (&b.uid, "bob")] {
            let bare = uid.strip_prefix("myclaw/u/").unwrap();
            let meta = data.join("users").join(bare).join("meta.json");
            assert!(meta.is_file(), "missing meta.json for {username}");
            let on_disk: User =
                serde_json::from_str(&std::fs::read_to_string(&meta).unwrap()).unwrap();
            assert_eq!(on_disk.username, username);
            assert_eq!(on_disk.uid, uid);
        }

        // 改名只改该用户的文件（其余 mtime/内容不变——这里以内容为准断言）。
        reg.set_username(&a.uid, "alicia").unwrap();
        let a_meta = data
            .join("users")
            .join(a.uid.strip_prefix("myclaw/u/").unwrap())
            .join("meta.json");
        let on_disk: User =
            serde_json::from_str(&std::fs::read_to_string(&a_meta).unwrap()).unwrap();
        assert_eq!(on_disk.username, "alicia");

        // 不产生 users.json（目录化写入不再写单文件）。
        assert!(!data.join("users.json").exists());

        // 重开后从目录读回全部 3 个用户（root 含迁移语义）。
        drop(reg);
        let re = UserRegistry::with_namespace(data, DEFAULT_NAMESPACE);
        assert_eq!(re.count(), 3);
        assert!(re.find_by_username("root").is_some());
        assert!(re.find_by_username("alicia").is_some());
        assert!(re.find_by_username("bob").is_some());
    }
}
