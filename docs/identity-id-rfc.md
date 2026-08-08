# 身份模型与统一 ID 层 RFC（Identity & Unified ID）

> **状态**:已定稿(2026-08-09,用户确认)
> **取代**:本 RFC 取代 `agent-messaging-rfc.md` §2.2「用户实体层(P4:邮箱 + uid 双标识 + 用户自助)」的身份标识部分;§2.2 的邮箱/SMTP/gate/好友机制部分继续有效。
> **关联**:`agent-messaging-rfc.md` §3 消息通道、§4 好友机制、§6 P4 验收(涉及 uid/昵称的条目以本 RFC 为准)。

## 0. 背景

P4 遗留一个结构性矛盾:**uid 被设计为用户自选句柄**(`/register <email> <uid>`,先到先得),同时被硬性规定为**不可变内部键**。自选句柄与不可变天生矛盾——用户想改句柄,但 user.id 是全部内部关联键(contacts/mailbox/sender/G44/UserProfile 路径),一变就断历史关联。

收敛结论:**键与标识分离**——拆成两个正交概念:

| 概念 | 性质 |
|---|---|
| **uid** | 系统分配、不可变、内部键;用户不可见、不可设 |
| **username** | 用户自设、全局唯一、可改;外部标识(显示/@提及/定位) |

同时删除 nickname:唯一可改的 username 已覆盖 nickname 全部用例(显示、@提及、定位),nickname 唯一存续价值「允许重名」正是全部复杂度来源(关系内过滤、多命中歧义、命令层禁用)——收敛为单一 username。

## 1. 目标模型

```
routing_key ──UserRegistry──→ User{ uid: <uuidv7>(系统分配,不可变)
                                 username: <用户自设>(全局唯一,可改)
                                 email: 唯一,可更换 }
   首次出现 rk = 无 User ──→ 调度层 gate 拦截,引导 /register 或 /link
   /register <email> [username]  创建新 User(uid 系统分配;username 可选,默认派生)
   /link <id|email>              绑定到已有 User(P3 验证码机制复用)
   /email set <email>            更换邮箱(唯一占用即拒绝)
   /username set <x>             设置/更换 username(唯一占用即拒绝)
```

**生命周期与状态**(沿用 P4 已拍板):

- 无 pending/匿名状态:User 只在注册或绑定成功时创建(active)。新 rk 未注册 = 无 User = gate 拦截引导,不占位。
- gate 白名单不变:`/register`、`/email`、`/username`、`/link`、`/link_confirm`、`/help`、`/whoami`。
- 邮箱验证(混合):沿用 P4——SMTP 有则验证码,无则声明即生效。
- 唯一性:email 小写归一化全局唯一;username 全局唯一,占用即拒绝,保留字 `root`(系统首个用户固定 `username=root`)。
- root 锚点:系统首个用户固定 `uid=<uuidv7>(迁移时分配)` + `username=root`,存量身份全部归入 root(同 P4 迁移策略)。

## 2. 统一 ID 层 `<namespace>/<type>/<uuidv7>`

**动机**:P4 已确立 FQID 三段式 `<namespace>/<类型>/<实例>`。本 RFC 把「实例」从自选句柄升级为 **uuidv7(系统分配)**,类型段从「用户/频道/群组/机器人」扩展为**全实体注册表**,并统一为一个 parser。

**格式**:`<namespace>/<type>/<uuidv7>`,如 `myclaw/u/0196f2c8-xxxx-7xxx-xxxx-xxxxxxxxxxxx`。

- `<namespace>`:配置项 `[system] namespace`(默认 `myclaw`,可改实例名/品牌)。
- `<type>`:实体类型段(注册表,见下)。
- `<uuidv7>`:UUID v7——**时间有序**(DB 插入友好、可排序)、**随机分量防碰撞**、128 位标准格式。

**双保险防重叠**:类型段保证异类实体字符串空间不相交(确定性)+ uuid 随机性防同类碰撞(概率)。裸 uuid 不出现在类型不明确的接口。

**类型段注册表**(当前落地 + 预留):

| 段 | 实体 | 落地 | 说明 |
|---|---|---|---|
| `u` | user | ✅ | uid(内部键),与 username 正交 |
| `t` | task | ✅ | 统一 tools/task 的 `task_{n}` 与 delegation 的 `del_<uuid>` |
| `msg` | 跨 agent 消息 | ✅ | 取代 `uuid::new_v4()` msg_id |
| `s` | session | ✅ | 取代 8-hex 随机/计数器 session id |
| `job` | cron job | ✅ | 取代 12-hex 时间截断 id |
| `mem` | memory | 预留 | memory 当前标识 = key(语义名);出现 ID 引用场景时启用,零迁移(load 补生成) |

**FQID parser(一个通吃)**:解析 `ns/type/uuid` → 校验 namespace 前缀 → 按类型段路由。非本 namespace 前缀一律不认(防注入/防外部 id 混入,沿用 P4 可验前缀)。

**适用范围**:所有全局唯一、跨重启稳定、需被引用的实体。**不适用**:

- 会话内消息 id(`ws-{seg}-{ms}`):仅 session 内唯一,DB 自增索引紧凑有序,uuid 化负收益。
- 语义名:username / agent name / skill name / memory key——用户可读名字,自选、可改,与 `<类型>/<uuid>` 正交,不参与 uuid 层。
- 纯进程内句柄(shell proc_id `bg_*`、ephemeral session 等):运行期局部唯一即可。

**实施标注**:

- `Cargo.toml` uuid crate 加 `v7` feature(当前 `features = ["v4"]`)。
- 新建 `src/ids/` 模块(或等价位置):`UuidV7::new()` helper、`Fqid` 类型(parse/format/type 段校验)、类型段常量。
- 各实体 ID 生成点统一走 helper:
  - `storage/json_file.rs` `generate_session_id`(rand u32 8-hex)→ `myclaw/s/<uuidv7>`
  - `agents/session/backend.rs` InMemoryBackend counter → 同上(与文件 backend 同源,测试与生产一致)
  - `tools/task.rs` TaskState `task_{n}` → `myclaw/t/<uuidv7>`(next_id 退役)
  - `agents/delegation_coordinator.rs` `del_{uuid}` → `myclaw/t/<uuidv7>`
  - `tools/send_message.rs` / `tools/friends.rs` msg_id `uuid::new_v4()` → `myclaw/msg/<uuidv7>`
  - `agents/scheduling/scheduler.rs` `generate_id`(12-hex 纳秒截断)→ `myclaw/job/<uuidv7>`

## 3. 身份标识规则

### 3.1 uid(内部键,系统分配)

- 生成:uuidv7,注册时由系统分配,用户不可选、不可见、不可改。
- 存储/引用:users.json 键、contacts 表键、mailbox 键、`sender_user_id`、G44 会话发现、UserProfile 路径(`workspace/users/{uid}/`)、agent 上下文 `<ref id="myclaw/u/<uuidv7>"/>`——一律 uid。
- 与 username 解耦:改 username 绝不影响任何内部关联。

### 3.2 username(外部标识,用户自设)

- 规则:`[a-z0-9_]+`,3–32 位,小写归一化,全局唯一,保留字 `root`;可改(`/username set`,占用即拒绝)。
- 展示:用户可见身份引用统一 `@username`(唯一,无需附带 id 消歧)。
- 定位:建关系(/friend_request 等)与 @提及均可用 username 定位——全局唯一,无重名歧义。

### 3.3 删除 nickname

- User 结构删 `nickname` 字段;`/nickname set` 命令移除(由 `/username set` 取代);UserMail.sender_nickname 字段名保留(纯显示字段,值随 display 为 `@username` 形态)。
- 显示回退:无 username 时回退派生名(如渠道名/`u/<uuid 短尾>`),不回退 nickname。
- mention 昵称分支(`@昵称` 关系内比对)删除——`@username` 全局解析替代。

## 4. @ 提及语义

- **@ 专提人**:主流软件(微信/QQ/Telegram/Discord/GitHub)@ 均提用户,无人做统一实体提及符 → 撤销 P4 §2.2 的「@ 统一实体提及符」设计(YAGNI,为不存在的实体做的理论洁癖)。
- **`@username` 全局解析**:username 全局唯一 → 不再限定「已建立关系内」;UserRegistry 精确解析,查不到即「未找到 @username」。
- **好友校验不在此处**:解析层单一职责(提及→uid),「你们还不是好友」由 send_message 工具内 contacts 检查拦截(沿用 P4)。
- **`u/` 类型段仅内部**:`<ref id="myclaw/u/<uuidv7>"/>` 只存在于 agent 上下文与存储;用户可见一律 `@username`。
- 入站 @ 预解析(MentionPreParse,chain 第 6 位)沿用:扫描 `@username` → 解析为 `<ref id="…"/>` 进 agent 上下文。

## 5. 配置改名

- `[messaging] namespace` → **`[system] namespace`**:语义从「messaging 专属」扩展为「全局 ID 命名空间/实例标识」;`[messaging]` 段回归纯粹(只剩 smtp)。
- `MessagingConfig` 拆出 namespace 字段,新 `[system]` 段(或等价位置)持有。
- 存量迁移:**零**——实测 myclaw.toml 无 `[messaging]` 段,纯代码级改名。

## 6. 数据迁移(5 项 + 1 项可选清理)

**原则**:启动自动迁移(结构版本化 + 备份),手动命令覆盖管理员改 namespace 场景。全部迁移前留 `.bak` 备份。

### 6.1 users.json v1 → v2(启动自动)

- v1:`{root: {uid: "root", active, created_ms}}` → v2:`{root: {uid: "<uuidv7>", username: "root", active, created_ms, version: 2}}`
- root 的 uid 从 `"root"` 改为系统分配的 uuidv7(迁移时生成一次并固定);`username=root` 补默认。
- 迁移前 `users.json.v1.bak`。

### 6.2 user_resolver.json 重写(启动自动,与 6.1 同事务)

- 7 条 `rk → myclaw/u/root` 的 FQID 中 `root` 段同步改写为迁移后的 uuidv7。
- 迁移前 `user_resolver.json.v1.bak`。

### 6.3 sessions 目录重命名(启动自动)

- 存量 **147 个** 8-hex 目录(`sessions/{8-hex}/`):每个生成 `myclaw/s/<uuidv7>` → 目录重命名 + 改写 `meta.json` 的 `id` 字段 + 重写 `active.json` 的 session_id 值(owner 键不动)。
- 迁移前整目录备份(sessions/ → sessions.bak/ 或逐目录 .bak)。
- 顺带根治 32 位随机 8-hex 的碰撞隐患(147 存量下碰撞概率已 ~2.6e-6,数千个后 ~0.3%)。

### 6.4 tasks.json 重写(启动自动)

- 存量 **13 条** `task_{n}`:逐条生成 `myclaw/t/<uuidv7>`,`parent_id` 同步重写(旧 id → 新 id 映射表),`next_id` 字段退役。
- 迁移前 `tasks.json.bak`。
- 旧对话上下文中已出现的 `task_2` 等引用按新 id 查不到——一次性重写,代价极小,接受。

### 6.5 cron/jobs.json 重写(启动自动)

- 存量 **1 条** `07fcb1d780eb` → `myclaw/job/<uuidv7>`;`run_logs/07fcb1d780eb.jsonl` 同步改名(历史日志不迁移,留档)。
- 迁移前 `jobs.json.bak`。

### 6.6 users/ 遗留 rk 目录归档(可选清理,默认执行)

- `workspace/users/` 下 **10 个** rk 格式遗留目录(`telegram:myclaw:6270938644`、`qqbot:xiaosan:…`、`client:default:web:…`、`telegram_myclaw_6270938644` 等)为死数据——当前代码经 resolver 解析后只读写 `users/myclaw/u/<uuid>/memory/`。
- 处置:启动时统一挪入 `users/.legacy-rk-archive/`(不动内容),不合并进 root(数据考古非迁移必需)。

### 6.7 手动命令 `myclaw migrate-namespace <new>`

- 场景:管理员改 `[system] namespace`(如 `myclaw` → 品牌名)。
- 流程:备份 → 干跑(列出将重写的文件与条目数)→ 确认 → 执行(重写 users.json / resolver / sessions meta / 各 ID 的 namespace 段)。
- 实现:CLI 子命令,复用 6.1–6.5 的迁移引擎(参数化为目标 namespace)。

## 7. 命令面变更

| 命令 | 变更 |
|---|---|
| `/register <email> [username]` | uid 参数移除(系统分配);username 可选,默认派生 |
| `/username set <x>` | **新增**,替代 `/nickname set`;全局唯一校验 |
| `/nickname set <x>` | **移除**(命令、帮助、catalog 全部删除) |
| `/whoami` | 展示 uid(短尾)+ username + email |
| `/link` `/link_confirm` | 不变(target 为 user.id 或 email) |

## 8. 验收清单

- [x] uuidv7 系统 uid:注册不再接受自选 uid;User.uid 恒为 `myclaw/u/<uuidv7>`
- [x] username 唯一:占用即拒绝;`/username set` 可更换;保留字 root
- [x] nickname 全删:字段、命令、mention 昵称分支、render 回退(UserMail.sender_nickname 字段名保留,值随 display 为 `@username` 形态)
- [x] `@username` 全局解析;`u/` 输入形态保留(命令/工具参数按 uid 内部键优先、username 回退)
- [x] FQID parser 通吃 u/t/msg/s/job;裸 uuid 不出现在类型不明确接口
- [x] session/task/msg/cron id 全部 `<ns>/<type>/<uuidv7>`
- [ ] `[system] namespace` 配置生效;`[messaging]` 只剩 smtp
- [ ] 启动自动迁移 5 项 + 可选清理,全部留 .bak;`migrate-namespace` 干跑/确认/执行
- [ ] 全量测试 + clippy -D warnings 通过(CI)
- [ ] 部署(update + 热切换 + doctor)

## 9. 数学附录

- uuidv7 时间戳 48 位 + 随机 74 位:同类碰撞概率与 v4 同量级(122 位熵量级,50% 碰撞需 ~2.7×10^18 个 ID)。
- MyClaw 10 万实体量级碰撞概率 ~10^-27,可忽略;类型段另提供确定性防重叠(双保险)。
