# MyClaw Memory 系统设计方案

## 1. 背景与目标

### 1.1 现状问题

| 问题 | 说明 |
|------|------|
| 纯内存存储 | `MemoryStore` 是 `HashMap`，进程重启全丢 |
| 无自动提取 | 完全依赖 LLM 主动调 `memory_store`，实际几乎不调用 |
| 与 bootstrap 割裂 | `MEMORY.md` 作为 bootstrap file 加载，但与 `MemoryStore` 完全无关 |
| 无索引机制 | 即使有 memory 文件，也无法按需检索 |
| 单 session 静态 | bootstrap files 只在会话开始时读一次，跨 session 不感知变更 |

### 1.2 目标

建立**文件级持久记忆 + compaction 时自动提取 + 跨 session 动态同步**的系统：

1. 跨会话记住用户偏好、行为纠正、项目决策
2. 不依赖 LLM 专用工具，由 compaction 触发自动提取
3. 记忆索引动态注入，跨 session 实时同步
4. 每次 compaction 时顺便做记忆的增删合并，防止无限积累

### 1.3 设计原则

- **不引入新工具**：记忆操作复用 `file_read`/`file_write`/`file_edit`
- **不增加 LLM 调用**：记忆提取嵌入 compaction，零额外开销
- **索引 = 文件列表**：无独立索引文件，从 `memory/*.md` 的 frontmatter 动态生成
- **跨 session 感知**：file watcher 监听 `memory/` 目录，变更触发 system-reminder 注入
- **容错**：记忆操作失败不影响 compaction，frontmatter 格式错误不崩溃

---

## 2. 架构设计

### 2.1 存储结构

```
~/.myclaw/workspace/
├── USER.md                ← 用户基本事实（名字、城市、职业），手动维护
├── AGENTS.md              ← 操作规则，手动维护
├── SOUL.md                ← AI 身份和态度，手动维护
└── memory/                ← 记忆文件目录（LLM 写入，后端扫描生成索引）
    ├── user_role.md
    ├── feedback_no_diff.md
    ├── project_auth_rewrite.md
    └── reference_linear_ingest.md
```

**没有 MEMORY.md 文件。** 索引在运行时从 `memory/*.md` 的 frontmatter 动态生成，不存在索引与文件不一致的问题。

### 2.2 文件职责边界

| 文件 | 内容 | 写入者 | 变化频率 |
|------|------|--------|----------|
| USER.md | 基本事实：名字、城市、职业 | 手动 | 极低 |
| AGENTS.md | 操作规则 | 手动 | 低 |
| SOUL.md | AI 身份和态度 | 手动 | 极低 |
| memory/*.md | 记忆文件（带 frontmatter） | LLM（compaction + 主动） | 中 |

**不重复原则**：memory 里不存 USER.md 已有的信息。USER.md 说"Albert，上海，程序员"就够了。

### 2.3 记忆文件格式

每个 `memory/*.md` 文件：

```markdown
---
name: user_language
description: 用户要求中文回复
type: user
created_at: 2026-05-07
---

用户偏好使用中文交流。所有回复都应该是中文，包括代码注释和技术说明。
```

**frontmatter 字段**：

| 字段 | 必填 | 说明 |
|------|------|------|
| `name` | ✅ | 文件标识，由 LLM 生成。同时也是文件名（`name: user_language` → `memory/user_language.md`） |
| `description` | ✅ | 一句话描述（≤150字符），用于索引展示和去重 |
| `type` | ✅ | `user` / `feedback` / `project` / `reference` |
| `created_at` | ✅ | 创建日期 `YYYY-MM-DD` |

**type 分类**：

| type | 说明 | 内容结构 |
|------|------|----------|
| `user` | 用户偏好、习惯 | 直接陈述 |
| `feedback` | 行为纠正 | 先写规则，再写 **Why:** 和 **How to apply:** |
| `project` | 项目决策背景 | 先写事实，再写 **Why:** 和 **How to apply:** |
| `reference` | 外部系统引用 | 直接陈述 |

**明确排除**（不存入 memory）：
- 代码模式、文件路径、函数名
- 架构信息、git history
- 可以从代码/文件推导的信息

**容错**：如果 frontmatter 缺失或格式错误，scanner 跳过该文件，不崩溃。

---

## 3. 索引生成与注入

### 3.1 动态索引生成

不维护独立的索引文件。每次需要展示索引时，扫描 `memory/*.md` 目录：

```rust
fn build_memory_index(workspace_dir: &str) -> Vec<IndexEntry> {
    let memory_dir = Path::new(workspace_dir).join("memory");
    let mut entries = Vec::new();
    for entry in fs::read_dir(&memory_dir) {
        let path = entry.path();
        if path.extension() != Some(OsStr::new("md")) { continue; }
        if let Some(file) = parse_memory_file(&path) {
            entries.push(IndexEntry {
                mem_type: file.mem_type,
                name: file.name,
                filename: path.file_name().unwrap().to_str().unwrap().to_string(),
                description: file.description,
            });
        }
    }
    // 按 type 分组，每组内按 name 字母序
    entries.sort_by(|a, b| (&a.mem_type, &a.name).cmp(&(&b.mem_type, &b.name)));
    entries
}
```

生成的索引文本格式：

```
## user
- user_role.md — 用户是 Python 后端工程师，用 Telegram 交互
- user_language.md — 用户要求中文回复

## feedback
- no_diff_summary.md — 不要在回复末尾总结 diff

## project
- auth_rewrite.md — auth 中间件重写因合规要求
```

### 3.2 System Prompt 注入（静态部分）

从 bootstrap files 列表中移除 `"MEMORY.md"`，新增 `build_memory_section()`：

```
## Memory

你有文件级持久记忆系统，文件存放在 `memory/` 目录。
记忆按 type 分类：user（用户偏好）、feedback（行为纠正）、project（项目背景）、reference（外部引用）。
当记忆内容与当前任务相关时，用 file_read 读取详细文件。

如果用户明确要求记住某事，或你发现偏好/行为模式变化，用 file_write 写入 memory/ 目录。
文件必须包含 YAML frontmatter（name / description / type / created_at）。
不要存可以从代码/文件推导的信息（代码路径、架构、git history）。

### 记忆索引

（此处动态插入从 memory/*.md 生成的索引。
如果 memory/ 目录为空，显示"暂无记忆"。）
```

**截断保护**：索引部分超过 200 行或 25KB 时截断，加警告注释。

### 3.3 跨 Session 动态同步（system-reminder）

**问题**：MyClaw 是多 session daemon。Session A 的 compaction 写了新记忆，Session B 不知道。

**方案**：复用现有 `AttachmentManager` + `check_changes()` 机制，监听 `memory/` 目录变更。

#### 3.3.1 File Watcher 扩展

当前 `check_changes()` 已经通过 file watcher 监听 skills/agents 目录变更。扩展为同时监听 `memory/` 目录：

```rust
// watcher 同时监控 memory/ 目录
if changes.memory_changed {
    let new_index = build_memory_index(&self.workspace_dir);
    let history = self.session.history.clone();
    self.attachments.diff_memory(&new_index, &history);
    tracing::info!(memory_count = new_index.len(), "memory hot-reloaded");
}
```

#### 3.3.2 AttachmentManager 扩展

在 `AttachmentManager` 中增加 memory 索引状态：

```rust
struct AttachmentManager {
    // 现有字段...
    memory_index: Option<String>,   // 当前注入过的 memory 索引文本
}

impl AttachmentManager {
    /// 比较 memory 索引变化，生成 system-reminder 消息
    fn diff_memory(&mut self, new_index: &[IndexEntry], history: &[ChatMessage]) {
        let new_text = format_memory_index(new_index);
        let old_text = self.memory_index.take().unwrap_or_default();

        if new_text == old_text {
            self.memory_index = Some(old_text);
            return;  // 无变化
        }

        // 检查 history 中是否已有相同的 system-reminder，避免重复注入
        if !history_contains_memory_reminder(history, &new_text) {
            let msg = format!(
                "<system-reminder>\n## Memory Index Updated\n\n{}\n</system-reminder>",
                new_text
            );
            self.pending.insert("memory", msg);
        }

        self.memory_index = Some(new_text);
    }
}
```

#### 3.3.3 注入时机

| 场景 | 触发 | 效果 |
|------|------|------|
| 会话开始 | `build_memory_section()` | 静态索引写入 system prompt |
| 同 session compaction 写了记忆 | compaction 完成 → 直接更新 `memory_index` | 下一轮 `build_messages()` 时注入 system-reminder |
| 其他 session 写了记忆 | file watcher 检测到 `memory/` 变更 | `check_changes()` → `diff_memory()` → system-reminder |

**system-reminder 格式**：

```
<system-reminder>
## Memory Index Updated

## user
- user_role.md — 用户是 Python 后端工程师
- user_language.md — 用户要求中文回复

## feedback
- no_diff_summary.md — 不要在回复末尾总结 diff
</system-reminder>
```

---

## 4. Compaction 时自动提取

### 4.1 触发时机

每次 compaction 时，LLM 在 summarizer 里直接用 `file_write`/`file_edit` 操作记忆文件。

当前 compaction 流程中 `do_inline_summarize` 已经支持多轮工具调用（最多 10 轮），工具执行器已经就绪。

### 4.2 Summarizer Instruction 改动

在现有 summarizer instruction 末尾追加：

```
You also have a persistent memory system. The memory directory is `memory/` and 
its current index is in your system prompt above.

Based on this conversation, decide if any memories should be saved, updated, or 
deleted. Use file_write to create/update memory files and shell (rm) to delete them.

Each memory file MUST have YAML frontmatter:
---
name: short_snake_case_name
description: one-line description (under 150 chars)
type: user|feedback|project|reference
created_at: YYYY-MM-DD
---

Then the memory content in markdown.

Rules:
- ONLY save things NOT derivable from code/git (user preferences, decisions, corrections)
- Check the existing memory index to avoid duplicates — update existing files instead of creating duplicates
- If existing memories are outdated or contradicted, update or delete them
- Keep name short, lowercase, underscores (becomes the filename: memory/{name}.md)
- If no memory changes needed, skip this entirely and just output the summary

You may use file_write and file_edit tools for memory operations ONLY. Do not use other tools.
```

**关键设计决策**：

1. **LLM 直接用工具写记忆文件**：复用现有工具执行器，零新代码
2. **不需要 JSON 解析**：没有 structured output，没有 `parse_memory_decision`
3. **不需要 `rebuild_index`**：没有 MEMORY.md 文件，索引从文件动态生成
4. **LLM 可以先读后写**：`file_read` 已有文件 → 判断是否需要更新 → `file_write`

### 4.3 Compaction 流程

```
do_inline_summarize()
→ 构建 messages（复用 system prompt，含 memory 索引）
→ 追加 summarizer instruction（summary + memory 操作指令）
→ mini chat_loop（LLM 可能先用 file_write 写记忆文件，最后输出 summary text）
→ compaction 完成
→ 后端调用 diff_memory() 更新当前 session 的索引视图
→ 其他 session 通过 file watcher 收到变更通知
```

**容错**：
- LLM 不写任何记忆 → 正常，只有 summary
- LLM 写了格式错误的 frontmatter → scanner 跳过，不影响 compaction
- file_write 失败 → LLM 可以重试或跳过，不影响 summary 生成

### 4.4 去重与合并

LLM 在 compaction 时已经能看到 system prompt 中的记忆索引（当前所有记忆文件的列表）。它自己判断：
- 已有相似记忆 → `file_edit` 更新内容
- 多个相似记忆 → `file_write` 写合并后的文件 + `rm` 删除旧的
- 过时记忆 → `rm` 删除

后端不做额外去重。LLM 比规则引擎更适合做这个判断。

---

## 5. Agent 主动维护

System prompt 中的 Memory section 告诉 Agent 操作方式（见 3.2）。

Agent 可以在正常对话中主动写入/更新/删除记忆文件，不需要等 compaction：
- `file_write memory/feedback_xxx.md` — 新增/覆盖
- `file_edit memory/feedback_xxx.md` — 局部更新
- `shell rm memory/feedback_xxx.md` — 删除

写入后，file watcher 检测到变更，其他 session 通过 system-reminder 收到更新。

---

## 6. 详细设计

### 6.1 新增模块 `src/memory/mod.rs`

```rust
// --- 数据类型 ---

pub struct MemoryFile {
    pub name: String,
    pub description: String,
    pub mem_type: MemoryType,
    pub created_at: String,
    pub content: String,
    pub path: PathBuf,
}

pub enum MemoryType { User, Feedback, Project, Reference }

pub struct IndexEntry {
    pub mem_type: MemoryType,
    pub name: String,
    pub filename: String,       // 如 "user_role.md"
    pub description: String,
}

// --- 核心函数 ---

/// daemon 启动时调用，确保 memory/ 目录存在
pub fn ensure_memory_dir(workspace_dir: &str) -> std::io::Result<PathBuf>

/// 扫描 memory/*.md，解析 frontmatter，返回所有有效记忆文件
/// frontmatter 缺失或格式错误的文件被跳过
pub fn scan_memory_files(memory_dir: &Path) -> Vec<MemoryFile>

/// 从记忆文件列表生成索引文本（按 type 分组，每组内按 name 字母序）
pub fn format_memory_index(entries: &[IndexEntry]) -> String

/// 解析单个 .md 文件的 YAML frontmatter + content
fn parse_memory_file(path: &Path) -> Option<MemoryFile>

/// 截断保护：超过 max_lines 或 max_bytes 时截断
pub fn truncate_index(content: &str, max_lines: usize, max_bytes: usize) -> String
```

**没有** `rebuild_index`、`write_memory_index`、`parse_memory_decision`、`execute_memory_decision`、`MemoryDecision`、`NewMemory`、`UpdateMemory`、`MergeOperation`。

索引 = 文件列表，记忆操作 = LLM 直接用工具。

### 6.2 System Prompt 改动（`prompt.rs`）

**移除**：从 `build_bootstrap_files()` 的文件列表中删除 `"MEMORY.md"`

**新增**：`build_memory_section()` 方法

```rust
fn build_memory_section(&self) -> String {
    // 1. workspace_dir 为空则返回空
    // 2. 调用 scan_memory_files() + format_memory_index()
    // 3. 拼装：操作指令（静态文本）+ 索引内容（动态生成）
    // 4. truncate_index() 截断保护
    // 5. memory/ 不存在或为空时显示"暂无记忆"
}
```

### 6.3 File Watcher 扩展（`agent_impl.rs`）

在 `check_changes()` 中增加 `memory/` 目录变更检测：

```rust
if changes.memory_changed {
    let memory_dir = Path::new(&self.workspace_dir).join("memory");
    let entries = memory::scan_memory_files(&memory_dir)
        .into_iter()
        .map(|f| memory::IndexEntry { ... })
        .collect();
    let history = self.session.history.clone();
    self.attachments.diff_memory(&entries, &history);
}
```

### 6.4 AttachmentManager 扩展（`attachment.rs`）

```rust
// 新增字段
memory_index: Option<String>,

// 新增方法
pub fn diff_memory(&mut self, new_entries: &[IndexEntry], history: &[ChatMessage])
```

### 6.5 Compaction 改动（`agent_impl.rs`）

**`do_inline_summarize`** 改动：

1. 在 summarizer instruction 末尾追加 memory 操作指令（见 4.2）
2. 将 "Do NOT use any tools" 改为 "You may use file_write and file_edit for memory operations ONLY"
3. mini chat_loop 不变——LLM 可能用几轮写记忆文件，最后一轮输出 summary
4. compaction 完成后，调用 `diff_memory()` 更新当前 session 的索引视图

**改动范围**：
- `do_inline_summarize`：修改 prompt 文本
- 不改 compact_impl / maybe_compact / summarize_inline 的调用链

### 6.6 File Watcher 注册（`daemon.rs`）

启动时将 `memory/` 目录加入 watcher：

```rust
if let Ok(memory_dir) = memory::ensure_memory_dir(&config.workspace_dir) {
    watcher.watch(&memory_dir)?;  // 已有 watcher 实例
}
```

---

## 7. 文件改动清单

| 文件 | 操作 | 说明 |
|------|------|------|
| `src/memory/mod.rs` | **新增** | frontmatter 解析、索引生成、截断保护 |
| `src/agents/prompt.rs` | **改** | 移除 MEMORY.md from bootstrap，新增 `build_memory_section()` |
| `src/agents/agent_impl.rs` | **改** | compaction prompt + check_changes 扩展 |
| `src/agents/attachment.rs` | **改** | 新增 `diff_memory()` + `memory_index` 字段 |
| `src/daemon.rs` | **改** | 启动时 `ensure_memory_dir` + watcher 注册 |
| `src/tools/memory.rs` | **删除** | 旧 HashMap 工具不再需要 |
| `src/config/memory.rs` | **简化** | 去掉 storage/embedding/consolidation 配置 |
| `workspace/AGENTS.md` | **改** | 移除 MEMORY.md / memory_tools 引用，更新为新的 memory 操作说明 |
| `workspace/MEMORY.md` | **删除** | 不再需要索引文件 |

---

## 8. 实施顺序

### Phase 1：基础设施（可独立验证）

1. 创建 `src/memory/mod.rs`：`ensure_memory_dir` + `scan_memory_files` + `format_memory_index` + `parse_memory_file` + `truncate_index`
2. 改 `src/agents/prompt.rs`：移除 MEMORY.md from bootstrap + 新增 `build_memory_section()`
3. 改 `src/daemon.rs`：启动时 `ensure_memory_dir`

**验证**：LLM 看到 Memory section + 索引，能用 `file_read` 读 memory 文件，能用 `file_write` 写新记忆文件

### Phase 2：跨 Session 同步

4. 改 `src/agents/attachment.rs`：新增 `memory_index` 字段 + `diff_memory()` 方法
5. 改 `src/agents/agent_impl.rs`：`check_changes()` 增加 memory 变更检测
6. 改 `src/daemon.rs`：watcher 注册 `memory/` 目录

**验证**：Session A 写记忆文件 → Session B 下一轮收到 system-reminder 更新

### Phase 3：Compaction 集成

7. 改 `src/agents/agent_impl.rs`：`do_inline_summarize` 追加 memory 操作指令 + 放开工具限制
8. compaction 完成后调用 `diff_memory()` 更新当前 session

**验证**：compaction 触发时 LLM 自动写记忆文件，其他 session 感知变更

### Phase 4：清理

9. 删除 `src/tools/memory.rs`
10. 从 `tools/mod.rs` 移除相关导出
11. 简化 `src/config/memory.rs`

**验证**：编译通过，旧工具不在 tool list 中

### Phase 5：Workspace 文件更新

12. 更新 `AGENTS.md`：

| 行 | 当前内容 | 改为 |
|------|------|------|
| L11 | `Use memory_recall for recent context` | 删除（旧工具已移除） |
| L12 | `If in MAIN SESSION: MEMORY.md is already injected` | 删除（MEMORY.md 不再存在） |
| L16-38 | Memory section（MEMORY.md + memory tools） | 替换为新的 memory 操作说明：记忆在 `memory/` 目录，用 file_write/file_edit 操作，带 frontmatter |
| L57 | `重要任务进展写入 MEMORY.md` | `重要任务进展写入 memory/ 目录` |
| L89 | `Check MEMORY.md + latest memory/*.md notes` | `Check memory/ 目录下的记忆文件` |

13. 删除 `MEMORY.md`（如果存在）

**验证**：AGENTS.md 无 MEMORY.md 引用，无 memory_recall/memory_store/memory_forget 引用

---

## 9. 已确认的决策

| 决策 | 结论 |
|------|------|
| MEMORY.md 文件 | **不需要**，索引从 memory/*.md 的 frontmatter 动态生成 |
| 索引维护方式 | **后端扫描 frontmatter 自动生成**，不依赖 LLM 更新索引 |
| USER.md 与 memory 的关系 | USER.md 只记基本事实，memory 作为增量 |
| name 字段 | 由 LLM 生成，同时用作文件名 |
| Compaction 中的记忆操作 | LLM 直接用 file_write/file_edit 工具，不输出 JSON |
| 跨 session 同步 | file watcher 监听 memory/ 目录 → system-reminder 注入 |
| 记忆操作失败 | log warning，不阻塞 compaction |
| frontmatter 格式错误 | scanner 跳过该文件，不崩溃 |

---

## 10. 与 Claude Code 对比

| 维度 | Claude Code | MyClaw |
|------|-------------|--------|
| 索引文件 | MEMORY.md（LLM 维护） | 无，动态生成 |
| 索引生成 | LLM 写两步（文件+索引） | 后端扫描 frontmatter |
| 跨 session | 单 session 桌面应用，不需要 | file watcher + system-reminder |
| 记忆提取 | 后台 fork agent | 嵌入 compaction summarizer |
| 索引加载 | session 级缓存（/clear 时刷新） | 每轮动态检测变更 |

---

## 11. 未来扩展

| 方向 | 说明 |
|------|------|
| 多用户隔离 | frontmatter 加 `user_id` 字段，scan 时过滤 |
| embedding 检索 | 替代简单索引匹配（config 里已有 `embedding_enabled` 预留） |
| team memory | 多用户共享的 memory 目录 |
| KAIROS 日志模式 | append-only 日志 + 夜间整理 |
