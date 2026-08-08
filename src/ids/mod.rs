//! 统一 ID 层（identity-id-rfc §2）。
//!
//! 全实体 ID 格式 `<namespace>/<type>/<uuidv7>`：
//! - `namespace`: 实例命名空间（`[system] namespace`，默认 `myclaw`）；
//! - `type`: 实体类型段（注册表见 `KNOWN_TYPES`）；
//! - `uuidv7`: 系统分配、时间有序、随机分量防碰撞。
//!
//! 双保险防重叠：类型段保证异类实体字符串空间不相交（确定性）+ uuidv7
//! 随机分量防同类碰撞（概率）。裸 uuid 不出现在类型不明确的接口。
//!
//! 不适用：语义名（username / agent name / skill name / memory key——
//! 用户可读名字，自选、可改，与 `<type>/<uuid>` 正交）；会话内消息 id
//! （仅 session 内唯一）；纯进程内句柄。

use uuid::Uuid;

/// 默认命名空间（`[system] namespace` 缺省值；存量数据与测试兼容）。
pub const DEFAULT_NAMESPACE: &str = "myclaw";

/// 类型段：user（uid，内部键）。
pub const TYPE_USER: &str = "u";
/// 类型段：task（tools/task 与 delegation 统一）。
pub const TYPE_TASK: &str = "t";
/// 类型段：跨 agent 消息。
pub const TYPE_MSG: &str = "msg";
/// 类型段：session。
pub const TYPE_SESSION: &str = "s";
/// 类型段：cron job。
pub const TYPE_JOB: &str = "job";
/// 类型段：memory（预留——当前 memory 标识 = key，语义名不参与 uuid 层；
/// 出现 ID 引用场景时启用，零迁移）。
pub const TYPE_MEMORY: &str = "mem";

/// 已知类型段注册表（解析/校验用；预留段也算已知）。
pub const KNOWN_TYPES: &[&str] = &[
    TYPE_USER,
    TYPE_TASK,
    TYPE_MSG,
    TYPE_SESSION,
    TYPE_JOB,
    TYPE_MEMORY,
];

/// 解析后的 FQID（`<namespace>/<type>/<uuidv7>`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fqid {
    namespace: String,
    type_seg: String,
    uuid: Uuid,
}

impl Fqid {
    /// 生成新 FQID（uuidv7）。
    pub fn new(namespace: &str, type_seg: &str) -> Self {
        Self {
            namespace: namespace.to_string(),
            type_seg: type_seg.to_string(),
            uuid: Uuid::now_v7(),
        }
    }

    /// 解析 `<ns>/<type>/<uuid>`。namespace 不匹配、段缺失或 uuid 非法 → None。
    pub fn parse(s: &str, namespace: &str) -> Option<Self> {
        let (ns, rest) = s.split_once('/')?;
        if ns != namespace {
            return None;
        }
        let (type_seg, uuid_str) = rest.split_once('/')?;
        if type_seg.is_empty() || uuid_str.is_empty() {
            return None;
        }
        let uuid = Uuid::parse_str(uuid_str).ok()?;
        Some(Self {
            namespace: ns.to_string(),
            type_seg: type_seg.to_string(),
            uuid,
        })
    }

    /// 类型段。
    pub fn type_seg(&self) -> &str {
        &self.type_seg
    }

    /// uuid（v7）。
    pub fn uuid(&self) -> Uuid {
        self.uuid
    }

    /// uuid 字符串（标准 36 字符小写）。
    pub fn uuid_str(&self) -> String {
        self.uuid.to_string()
    }

    /// 完整字符串形态 `<ns>/<type>/<uuid>`。
    pub fn as_str(&self) -> String {
        format!("{}/{}/{}", self.namespace, self.type_seg, self.uuid)
    }
}

impl std::fmt::Display for Fqid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}/{}", self.namespace, self.type_seg, self.uuid)
    }
}

/// 判断字符串是否为已知类型段（注册表内）。
pub fn is_known_type(type_seg: &str) -> bool {
    KNOWN_TYPES.contains(&type_seg)
}

/// id → 文件系统安全名（`/` → `_`、`_` → `__`；遗留短 id 无变化）。
///
/// 用于以 id 作目录/文件名的实体（session 目录、cron run_logs）——FQID 含
/// `/`，不能直接作为单段路径名。转义可逆（见 `id_from_dir`）。
pub fn dir_name(id: &str) -> String {
    id.replace('_', "__").replace('/', "_")
}

/// 文件系统安全名 → id（`dir_name` 逆操作）。
pub fn id_from_dir(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut chars = name.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '_' {
            if chars.peek() == Some(&'_') {
                out.push('_');
                chars.next();
            } else {
                out.push('/');
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_generates_parseable_fqid() {
        for ty in [TYPE_USER, TYPE_TASK, TYPE_MSG, TYPE_SESSION, TYPE_JOB] {
            let f = Fqid::new("myclaw", ty);
            let s = f.to_string();
            assert!(s.starts_with(&format!("myclaw/{ty}/")), "s={s}");
            let parsed = Fqid::parse(&s, "myclaw").expect("roundtrip");
            assert_eq!(parsed, f);
        }
    }

    #[test]
    fn uuid_is_v7() {
        let f = Fqid::new("myclaw", TYPE_USER);
        assert_eq!(f.uuid().get_version(), Some(uuid::Version::SortRand));
    }

    #[test]
    fn parse_rejects_wrong_namespace() {
        let s = Fqid::new("brand", TYPE_USER).to_string();
        assert!(Fqid::parse(&s, "myclaw").is_none());
    }

    #[test]
    fn parse_rejects_bad_uuid() {
        assert!(Fqid::parse("myclaw/u/not-a-uuid", "myclaw").is_none());
        assert!(Fqid::parse("myclaw/u/", "myclaw").is_none());
        assert!(Fqid::parse("myclaw/", "myclaw").is_none());
        assert!(Fqid::parse("myclaw", "myclaw").is_none());
    }

    #[test]
    fn known_types_include_all() {
        assert!(is_known_type(TYPE_USER));
        assert!(is_known_type(TYPE_MEMORY));
        assert!(!is_known_type("x"));
    }
}
