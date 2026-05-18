# MyClaw Skill 系统增强方案

> 状态：Draft
> 日期：2026-05-18
> 作者：张小二 & Albert

---

## 一、背景

MyClaw 当前 Skill 系统存在以下问题：

1. **工具集不完整**：只有 `use_skill`（加载全文），缺少列出 skill 目录的 `skills_list` 和创建/编辑/删除 skill 的 `skill_manage`
2. **Frontmatter 字段不足**：只解析 `name`、`description`、`keywords`，缺少 `version`、`when_to_use`、`argument_hint`、`arguments`、`user_invocable`、`agent_invocable`
3. **索引信息太弱**：attachment 只注入 name + description，模型难以判断是否该加载
4. **工具命名不统一**：`use_skill` 与 Hermes 的 `skill_view` 不一致

### 竞品参考

| 能力 | Claude Code | Codex | OpenClaw | Hermes | MyClaw (当前) |
|---|---|---|---|---|---|
| 列出 skill | ❌ (斜杠命令) | ❌ (自动注入) | ❌ (attachment) | ✅ `skills_list` | ❌ |
| 加载 skill | ✅ 斜杠命令 | ✅ mention 注入 | ✅ `read` 工具 | ✅ `skill_view` | ✅ `use_skill` |
| CRUD skill | ❌ | ❌ | ❌ | ✅ `skill_manage` | ❌ |
| 辅助文件读取 | ❌ | ❌ | ✅ `read` 工具 | ✅ `skill_view(file_path)` | ❌ |
| 模型自修复 skill | ❌ | ❌ | ❌ | ✅ patch + edit | ❌ |

---

## 二、设计目标

1. 三个工具：`skills_list`、`skill_view`、`skill_manage`，与 Hermes 对齐
2. 扩展 Frontmatter 字段，支持 `when_to_use`、`argument_hint` 等触发辅助信息
3. `agent_invocable` 控制模型是否能加载 skill；`user_invocable` 为未来斜杠命令预留
4. `skill_manage` 写操作后自动刷新 SkillManager 缓存
5. 索引注入增强：从 `name + description` 扩展为 `name + description + when_to_use`

---

## 三、Frontmatter 扩展

### 完整字段定义

```yaml
---
name: ctrip-flight                          # 必填，String
description: "Search for domestic flights"   # 必填，String
version: "1.2.0"                            # 可选，String
when_to_use: "用户想查机票、航班价格时"         # 可选，String
argument_hint: "[出发城市] [到达城市] [日期]"   # 可选，String
arguments: [from_city, to_city, date]        # 可选，List<String>
user_invocable: true                        # 可选，Bool，默认 true
agent_invocable: true                       # 可选，Bool，默认 true
keywords: [flights, 机票]                    # 可选，List<String>
---
```

### 各字段用途

| 字段 | 索引注入 | skill_view 返回 | skills_list 返回 | /skills 命令 | skill_manage 校验 |
|---|---|---|---|---|---|
| `name` | ✅ 加粗标题 | ✅ | ✅ | ✅ | ✅ create 唯一性 |
| `description` | ✅ 标题后 | ✅ | ✅ | ✅ | ✅ create/edit 必填 |
| `version` | ❌ | — | ✅ | ✅ | ❌ |
| `when_to_use` | ✅ `(trigger: ...)` | — | ✅ | ❌ | ❌ |
| `argument_hint` | ❌ | — | ✅ | ✅ | ❌ |
| `arguments` | ❌ | — | ❌ | ❌ | ❌ (未来 $arg 替换) |
| `user_invocable` | ❌ | — | ✅ | ✅ 标注 | ❌ |
| `agent_invocable` | ✅ 过滤依据 | ✅ 拒绝 false | ✅ | ✅ 标注 | ❌ |
| `keywords` | ❌ | — | ❌ | ✅ | ❌ |

---

## 四、数据结构变更

### 4.1 SkillDefinition（skill_loader.rs）

```rust
#[derive(Debug, Clone)]
pub struct SkillDefinition {
    // 已有
    pub name: String,
    pub description: String,
    pub keywords: Vec<String>,
    pub prompt_body: String,
    pub source_path: PathBuf,
    // 新增
    pub version: Option<String>,
    pub when_to_use: Option<String>,
    pub argument_hint: Option<String>,
    pub arguments: Vec<String>,
    pub user_invocable: bool,    // 默认 true
    pub agent_invocable: bool,   // 默认 true
}
```

### 4.2 Skill（skills.rs）

```rust
#[derive(Clone)]
pub struct Skill {
    // 已有
    pub name: String,
    pub description: String,
    pub keywords: Vec<String>,
    pub prompt_body: String,
    // 新增
    pub version: Option<String>,
    pub when_to_use: Option<String>,
    pub argument_hint: Option<String>,
    pub arguments: Vec<String>,
    pub user_invocable: bool,
    pub agent_invocable: bool,
}
```

### 4.3 SkillManager（skills.rs）

新增方法：

```rust
/// Iterate all skills (for skills_list).
pub fn skills_iter(&self) -> impl Iterator<Item = (&str, &Skill)> {
    self.skills.iter().map(|(k, v)| (k.as_str(), v))
}

/// Iterate only agent-invocable skills (for attachment injection).
pub fn agent_skills_iter(&self) -> impl Iterator<Item = (&str, &Skill)> {
    self.skills.iter()
        .filter(|(_, s)| s.agent_invocable)
        .map(|(k, v)| (k.as_str(), v))
}

/// Get skill directory path by name.
/// Returns the parent directory of source_path stored in the skill.
/// Note: requires source_path to be stored in Skill (see §4.4).
pub fn skill_dir(&self, name: &str) -> Option<&Path> {
    self.skills.get(name).and_then(|s| s.source_path.as_deref())
}

/// Replace all skills with a new set (used by refresh_skills after write operations).
pub fn reload(&mut self, new_skills: Vec<Skill>) {
    self.skills.clear();
    for skill in new_skills {
        self.skills.insert(skill.name.clone(), skill);
    }
}
```

### 4.4 Skill 增加存储目录路径

`skill_manage` 的 write_file/remove_file 需要知道 skill 目录路径。当前 `Skill` 不存 `source_path`，需要加上：

```rust
pub struct Skill {
    // ... 现有字段 ...
    /// Absolute path to the skill directory (parent of SKILL.md).
    /// Used by skill_manage to locate supporting files.
    pub skill_dir: Option<PathBuf>,
}
```

`from_definition` 中：
```rust
skill_dir: def.source_path.parent().map(|p| p.to_path_buf()),
```

---

## 五、工具实现

### 5.1 `skill_view`（改造现有 `use_skill`）

**文件**：`src/tools/skill_tool.rs`

**改动点**：
- tool name: `"use_skill"` → `"skill_view"`
- 新增 `file_path` 参数 — 读取辅助文件
- 新增 `agent_invocable` 校验
- 辅助文件不存在时返回目录中实际存在的文件列表

**Schema**：

```json
{
  "name": "skill_view",
  "description": "Load a skill's full instructions or a supporting file. Use this when the task matches a skill you see listed in system reminders. Returns the skill's complete behavioral guidance for you to follow.",
  "parameters": {
    "type": "object",
    "properties": {
      "name": {
        "type": "string",
        "description": "Name of the skill."
      },
      "file_path": {
        "type": "string",
        "description": "Optional. Relative path to a supporting file within the skill directory, e.g. 'scripts/ctrip_flight.py'. Omit to load the main SKILL.md."
      }
    },
    "required": ["name"]
  }
}
```

**execute 逻辑**：

```
1. 解析 name, file_path 参数
2. skills.read() 获取 SkillManager
3. skills.get(name) → 不存在则返回 error:
   {
     "success": false,
     "error": "Skill '{name}' not found.",
     "available_skills": ["skill-a", "skill-b", "skill-c"],
     "hint": "Use skills_list to see all available skills"
   }
4. 校验 agent_invocable:
   if !skill.agent_invocable:
       return Error("Skill '{name}' is not agent-invocable.")
5. if file_path 为空:
       // 加载主 SKILL.md — 返回完整 JSON (含 linked_files 导航)
       if skill.prompt_body.is_empty():
           return { success, name, content: "", message: "Skill has no instructions." }
       linked_files = scan_skill_files(skill.skill_dir)  // HashMap<String, Vec<String>>
       return {
           "success": true,
           "name": skill.name,
           "description": skill.description,
           "content": skill.prompt_body,
           "linked_files": linked_files,          // null if no skill_dir or no files
           "usage_hint": "To view linked files, call skill_view(name, file_path) \
                          where file_path is e.g. 'references/api.md' or 'scripts/run.py'"
                          if linked_files 非空 else null,
       }
   else:
       // 读取辅助文件
       skill_dir = skill.skill_dir → 没有 return Error
       防路径穿越: file_path 不能包含 ".."
       target = skill_dir / file_path
       if !target.starts_with(skill_dir):
           return Error("Path traversal not allowed")
       if !target.exists():
           // 列出实际存在的文件，按类型分组
           available_files = scan_skill_files(skill_dir)  // {"references": [...], "scripts": [...]}
           return {
               "success": false,
               "error": "File '{file_path}' not found in skill '{name}'.",
               "available_files": available_files,
               "hint": "Use one of the available file paths listed above"
           }
       content = std::fs::read_to_string(target)
       // 捕获二进制文件（图片等）
       match content:
           Ok(text) → return { success, name, file: file_path, content: text }
           Err(e) if e.kind() == InvalidData → return {
               success: true,
               name: skill.name,
               file: file_path,
               content: "[Binary file, cannot display as text]",
               is_binary: true,
               file_type: target.extension(),
               file_size: target.metadata().len(),
           }
           Err(e) → return Error("Failed to read file: {e}")
```

### 5.2 `skills_list`（新增）

**文件**：`src/tools/skills_list_tool.rs`

**struct**：

```rust
pub struct SkillsListTool {
    skills: Arc<RwLock<SkillManager>>,
}
```

**Schema**：

```json
{
  "name": "skills_list",
  "description": "List all available skills with metadata. Returns more detail than the auto-injected skill index. Use this when you need to browse skills, check if a specific skill exists, or see supporting files within a skill.",
  "parameters": {
    "type": "object",
    "properties": {}
  }
}
```

**execute 逻辑**：

```
1. skills.read() 获取 SkillManager
2. 收集所有 skill:
   for (name, skill) in skills.skills_iter():
       entry = {
           "name": skill.name,
           "description": skill.description,
       }
       if skill.when_to_use 非空: entry["when_to_use"] = ...
       if skill.argument_hint 非空: entry["argument_hint"] = ...
       if skill.version 非空: entry["version"] = ...
       if !skill.agent_invocable: entry["agent_invocable"] = false
       if !skill.user_invocable: entry["user_invocable"] = false

       // 列出辅助文件
       if skill.skill_dir 存在:
           files = scan skill_dir 下的 references/, scripts/, templates/, assets/
           if files 非空: entry["files"] = files
3. 按 name 排序
4. 返回 JSON:
   {
       "success": true,
       "count": N,
       "skills": [...entries...],
       "hint": "Use skill_view(name) to load full instructions, or skill_view(name, file_path) to read supporting files."
   }
```

**辅助文件扫描逻辑**：

```
fn scan_skill_files(dir: &Path) -> HashMap<String, Vec<String>> {
    let mut files = HashMap::new();
    for subdir in ["references", "scripts", "templates", "assets"]:
        let d = dir.join(subdir)
        if d.exists():
            let entries: Vec<String> = walk dir
                .filter(|f| f.is_file())
                .map(|f| f.relative_to(dir).to_string())
                .collect()
            if !entries.is_empty():
                files.insert(subdir, entries)
    files
}
```

### 5.3 `skill_manage`（新增）

**文件**：`src/tools/skill_manage_tool.rs`

**常量**：

```rust
const MAX_NAME_LENGTH: usize = 64;
const MAX_DESCRIPTION_LENGTH: usize = 1024;
const MAX_SKILL_CONTENT_CHARS: usize = 100_000;
const MAX_FILE_BYTES: usize = 1_048_576;  // 1 MiB
const VALID_NAME_REGEX: &str = r"^[a-z0-9][a-z0-9._-]*$";
const ALLOWED_SUBDIRS: &[&str] = &["references", "scripts", "templates", "assets"];
```

**Schema**：

```json
{
  "name": "skill_manage",
  "description": "Manage skills (create, edit, patch, delete). Skills are reusable approaches for recurring task types.\n\nActions: create (full SKILL.md), patch (old_string/new_string — preferred for fixes), edit (full rewrite — major overhauls), delete, write_file, remove_file.\n\nCreate when: complex task succeeded (5+ tool calls), errors overcome, user-corrected approach worked, or user asks to remember a procedure.\nUpdate when: instructions stale/wrong, missing steps or pitfalls found during use.\n\nGood skills include: trigger conditions, numbered steps with exact commands, pitfalls section, verification steps. Use skill_view() to see format examples.\n\nConfirm with user before creating or deleting skills. Skip for simple one-off tasks.",
  "parameters": {
    "type": "object",
    "properties": {
      "action": {
        "type": "string",
        "enum": ["create", "edit", "patch", "delete", "write_file", "remove_file"],
        "description": "Action to perform."
      },
      "name": {
        "type": "string",
        "description": "Skill name."
      },
      "content": {
        "type": "string",
        "description": "[create/edit] Full SKILL.md content including YAML frontmatter."
      },
      "old_string": {
        "type": "string",
        "description": "[patch] Text to find."
      },
      "new_string": {
        "type": "string",
        "description": "[patch] Replacement text. Use empty string to delete."
      },
      "file_path": {
        "type": "string",
        "description": "Path within the skill directory. For 'patch': optional, the file to patch (defaults to SKILL.md if omitted). For 'write_file'/'remove_file': required, must be under references/, templates/, scripts/, or assets/."
      },
      "file_content": {
        "type": "string",
        "description": "[write_file] Content for the supporting file."
      }
    },
    "required": ["action", "name"]
  }
}
```

**struct**：

```rust
pub struct SkillManageTool {
    skills: Arc<RwLock<SkillManager>>,
    workspace_dir: PathBuf,
}
```

**action 分发逻辑**：

```
match action:
    "create"      → action_create(name, content)
    "edit"        → action_edit(name, content)
    "patch"       → action_patch(name, old_string, new_string, file_path)
    "delete"      → action_delete(name)
    "write_file"  → action_write_file(name, file_path, file_content)
    "remove_file" → action_remove_file(name, file_path)
```

**action_create(name, content)**：

```
1. 校验参数: content 必填
2. validate_name(name):
   - 非空, ≤64 字符, 匹配 ^[a-z0-9][a-z0-9._-]*$
3. validate_frontmatter(content):
   - 必须以 --- 开头
   - 必须有闭合 ---
   - YAML 中必须有 name 字段
   - YAML 中必须有 description 字段
   - description ≤1024 字符
   - body 不能为空
4. validate_content_size(content): ≤100K 字符
5. 查重 + 内置保护:
   - name = "self" → 报错 "Cannot create skill with reserved name 'self'"
   - skills.get(name) → 已存在则报错
6. 创建目录: workspace_dir/skills/{name}/
7. 原子写入: workspace_dir/skills/{name}/SKILL.md
   (先写 .tmp 文件，再 std::fs::rename)
8. refresh_skills()
9. 返回:
   {
     "success": true,
     "message": "Skill '{name}' created.",
     "path": "skills/{name}/SKILL.md",
     "hint": "To add reference files, templates, or scripts, use skill_manage(action='write_file', name='{name}', file_path='references/...', file_content='...')"
   }
```

**action_edit(name, content)**：

```
1. 校验参数: content 必填
2. validate_frontmatter(content): 同 create 的步骤 3
3. validate_content_size(content)
4. skills.get(name) → 不存在则报错
5. 获取 skill_dir
6. 原子写入新内容到 SKILL.md
   (原子写入本身保证不会出现半写状态，无需额外备份)
7. refresh_skills()
8. 返回:
   {
     "success": true,
     "message": "Skill '{name}' updated."
   }
```

**action_patch(name, old_string, new_string, file_path)**：

```
1. 校验参数: old_string 和 new_string 必填 (new_string 可为空字符串)
2. skills.get(name) → 不存在则报错
3. 确定目标文件:
   - file_path 为空 → SKILL.md
   - file_path 非空 → validate_patch_file_path(file_path), 然后 skill_dir / file_path
4. 读取目标文件内容
5. 查找 old_string:
   - 匹配 0 次 → 返回 error + 文件前 500 字符预览
   - 匹配 >1 次 → 返回 error "old_string must be unique, found N matches"
6. 替换为新内容
7. 如果目标是 SKILL.md: validate_frontmatter(新内容)
8. validate_content_size(新内容)
9. 原子写入
10. refresh_skills()
11. 返回:
    {
      "success": true,
      "message": "Patched 1 replacement in {file_label}."
    }
```

**action_delete(name)**：

```
1. skills.get(name) → 不存在则报错
2. 不能删除内置 skill (name = "self")
3. 获取 skill_dir
4. std::fs::remove_dir_all(skill_dir)
5. refresh_skills()
6. 返回:
   {
     "success": true,
     "message": "Skill '{name}' deleted.",
     "path": "skills/{name}/"
   }
```

**action_write_file(name, file_path, file_content)**：

```
1. 校验参数: file_path 和 file_content 必填
2. validate_supporting_file_path(file_path):
   - 不能包含 ".."
   - 第一个路径段必须在 ALLOWED_SUBDIRS 中
   - 必须有文件名 (不只是目录)
3. validate_content_size(file_content): ≤100K 字符
4. 检查字节大小: ≤1 MiB
5. skills.get(name) → 不存在则报错
6. target = skill_dir / file_path
7. 路径安全: target 必须在 skill_dir 下 (resolve 后 starts_with)
8. 创建父目录
9. 原子写入
10. refresh_skills()
11. 返回:
    {
      "success": true,
      "message": "File '{file_path}' written.",
      "path": "skills/{name}/{file_path}"
    }
```

**action_remove_file(name, file_path)**：

```
1. 校验参数: file_path 必填
2. validate_supporting_file_path(file_path): 同上
3. skills.get(name) → 不存在则报错
4. target = skill_dir / file_path
5. 路径安全: 同上
6. if !target.exists():
       // 列出实际存在的文件供参考
       available = scan skill_dir 下 ALLOWED_SUBDIRS 中的文件
       return Error("File not found") + available
7. std::fs::remove_file(target)
8. 清理空父目录
9. 返回:
   {
     "success": true,
     "message": "File '{file_path}' removed from skill '{name}'."
   }
```

**validate_supporting_file_path(file_path)** — write_file / remove_file 专用：

```
1. 非空
2. 不包含 ".."
3. 解析路径: 第一段必须在 ALLOWED_SUBDIRS 中
4. 至少有两段 (子目录名 + 文件名)
```

**validate_patch_file_path(file_path)** — patch 专用，规则更宽松：

```
1. 不包含 ".."
2. 路径穿越检查: resolve 后 starts_with(skill_dir)
// patch 允许编辑 skill 目录下任意文件，不限于 ALLOWED_SUBDIRS
```

**refresh_skills() — 写后缓存刷新**：

```rust
fn refresh_skills(&self) {
    let skills_dir = self.workspace_dir.join("skills");
    let definitions = crate::agents::skill_loader::load_skills_from_dir(&skills_dir);
    let new_skills: Vec<Skill> = definitions.iter()
        .map(Skill::from_definition).collect();
    self.skills.write().reload(new_skills);
    // AttachmentManager 下次 diff_skills() 会通过 current set 对比自动检测变化
}
```

> **注意**：如果未来引入 system prompt 缓存（如 Hermes 的 LRU + 磁盘快照两层缓存），
> 需要在 `refresh_skills()` 之后联动调用缓存清除，确保 skill 索引不会读到过期内容。
> 当前 MyClaw 每次 attachment 重建都走 `diff_skills()` 实时对比，暂无缓存问题。

**原子写入实现细节**：

所有写操作使用同一原子写入函数：

```rust
/// 原子写入：先在同目录写 .tmp 文件，再 rename 到目标路径。
/// 如果进程在写入过程中崩溃，.tmp 文件不会影响原文件。
fn atomic_write(target: &Path, content: &str) -> std::io::Result<()> {
    let tmp = target.with_extension("tmp");
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, target)?;
    Ok(())
}
```

> 与 Hermes 的 `tempfile::mkstemp` 区别：Hermes 用随机后缀避免并发冲突。
> MyClaw 单进程单写者，`.tmp` 同名覆盖即可。

**辅助文件扫描**：

```rust
fn scan_skill_files(dir: &Path) -> HashMap<String, Vec<String>> {
    let mut files = HashMap::new();
    for subdir in ALLOWED_SUBDIRS {
        let d = dir.join(subdir);
        if d.exists() {
            if let Ok(entries) = std::fs::read_dir(&d) {
                let mut sub_files: Vec<String> = entries.flatten()
                    .filter(|e| e.path().is_file())
                    .filter_map(|e| e.path().strip_prefix(&d).ok()
                        .map(|p| p.to_string_lossy().to_string()))
                    .collect();
                if !sub_files.is_empty() {
                    sub_files.sort();
                    files.insert(subdir.to_string(), sub_files);
                }
            }
        }
    }
    files
}
```

---

## 六、Frontmatter 解析变更

### 6.1 str_utils.rs

新增：

```rust
/// Extract a boolean value from simple YAML text by key.
pub fn extract_yaml_bool(yaml: &str, key: &str) -> Option<bool> {
    extract_yaml_string(yaml, key).and_then(|v| match v.to_lowercase().as_str() {
        "true" | "yes" | "on" => Some(true),
        "false" | "no" | "off" => Some(false),
        _ => None,
    })
}
```

### 6.2 skill_loader.rs — parse_skill_file

在现有解析后新增：

```rust
let version = extract_yaml_string(&front_matter, "version");
let when_to_use = extract_yaml_string(&front_matter, "when_to_use");
let argument_hint = extract_yaml_string(&front_matter, "argument_hint");
let arguments = extract_yaml_list(&front_matter, "arguments");
let user_invocable = extract_yaml_bool(&front_matter, "user_invocable").unwrap_or(true);
let agent_invocable = extract_yaml_bool(&front_matter, "agent_invocable").unwrap_or(true);
```

> **注意**：`skill_manage` 的 `create`/`edit` 强制要求 `description` 必填。
> 但 `skill_loader` 加载已有文件时做容错处理：如果 frontmatter 缺少 `description`，
> 从 body 第一行非标题文本提取，最终兜底为空字符串。

### 6.3 skills.rs — Skill::from_definition

映射新增字段：

```rust
pub fn from_definition(def: &SkillDefinition) -> Self {
    Self {
        name: def.name.clone(),
        description: def.description.clone(),
        keywords: def.keywords.clone(),
        prompt_body: def.prompt_body.clone(),
        skill_dir: def.source_path.parent().map(|p| p.to_path_buf()),
        version: def.version.clone(),
        when_to_use: def.when_to_use.clone(),
        argument_hint: def.argument_hint.clone(),
        arguments: def.arguments.clone(),
        user_invocable: def.user_invocable,
        agent_invocable: def.agent_invocable,
    }
}
```

---

## 七、索引注入变更

### 7.1 调用链概述

索引注入涉及三个函数，形成完整数据流：

```
diff_skills(current = agent_skills_iter)
  → 计算 delta.added / delta.removed
    → render_skills(delta, skills)  // 只处理 delta 中的 skill
      → 写入 system prompt attachment
```

**关键变化**：`current` 集合从 `skills_iter()`（全量）改为 `agent_skills_iter()`（仅 agent 可调用）。

### 7.2 行为影响

| 场景 | 变化前 (skills_iter) | 变化后 (agent_skills_iter) |
|---|---|---|
| `agent_invocable=true` 的 skill | ✅ 出现在索引 | ✅ 出现在索引 |
| `agent_invocable=false` 的 skill | ✅ 出现在索引 | ❌ 不出现在索引 |
| skill 从 invocable→non-invocable | 不会被移除（仍在 current） | ✅ 触发 delta.removed，从索引移除 |
| skill 从 non-invocable→invocable | 不会被添加（已 announced） | ✅ 触发 delta.added，首次出现在索引 |

**`skills_iter()` vs `agent_skills_iter()` 的职责分工**：

| 方法 | 用途 | 消费者 |
|---|---|---|
| `agent_skills_iter()` | 计算模型可见 skill 集合 | `diff_skills` → system prompt 索引注入 |
| `skills_iter()` | 获取全部 skill 元数据 | `skills_list` 工具（模型主动查询全量） |

### 7.3 attachment.rs — diff_skills

```rust
pub fn diff_skills(&mut self, skills: &SkillManager, history: &[ChatMessage]) {
    let announced = Self::rebuild_from_history(history);
-   let current: HashSet<String> =
-       skills.skills_iter().map(|(n, _)| n.to_string()).collect();
+   // 只把 agent_invocable 的 skill 放入 current，
+   // 非 agent_invocable 的 skill 不会出现在模型的索引中。
+   let current: HashSet<String> =
+       skills.agent_skills_iter().map(|(n, _)| n.to_string()).collect();
    // ... 后续 diff 计算逻辑不变
    // delta.added = current - announced
    // delta.removed = announced - current
}
```

### 7.4 attachment.rs — render_skills

```rust
fn render_skills(delta: &Delta, skills: &SkillManager) -> String {
    let mut lines = vec!["## Skills".to_string()];

    // ... removed 处理不变 ...

    if !delta.added.is_empty() {
        lines.push(
            "Skills provide behavioral instructions for specific tasks. \
             Use the `skill_view` tool to load a skill's full instructions when needed."
                .to_string(),
        );
        for name in &delta.added {
            let skill = skills.get(name);
            let desc = skill.map(|s| s.description.as_str()).unwrap_or("");

            let mut parts = vec![format!("- **{}**", name)];
            if !desc.is_empty() {
                parts.push(format!(": {}", desc));
            }
            if let Some(s) = skill {
                if let Some(ref w) = s.when_to_use {
                    parts.push(format!(" (trigger: {})", w));
                }
            }
            lines.push(parts.join(""));
        }
    }

    lines.join("\n")
}
```

索引效果：

```
## Skills
Skills provide behavioral instructions...
- **ctrip-flight**: Search for domestic flights (trigger: 查机票、航班查询)
- **github**: Interact with GitHub using the `gh` CLI.
- **self**: Manage the myclaw daemon lifecycle...
```

---

## 八、/skills 命令变更

`slash_command.rs` — `format_skills_list`：

```rust
let ver = skill.version.as_deref().map(|v| format!(" v{}", v)).unwrap_or_default();
let invocable_mark = match (skill.user_invocable, skill.agent_invocable) {
    (true, true) => String::new(),
    (true, false) => " 👤".to_string(),   // 仅用户
    (false, true) => " 🤖".to_string(),   // 仅模型
    (false, false) => " 🚫".to_string(),  // 都不行
};
lines.push(format!("• **{}**{}{}{} — {}", name, ver, invocable_mark, kw, desc));
```

---

## 九、注册变更

### 9.1 daemon.rs — build_tools

```rust
// SkillTool — loads skill body on demand.
tools.register(Arc::new(crate::tools::SkillTool::new(Arc::clone(skills))));

+ // SkillsListTool — lists skill metadata.
+ tools.register(Arc::new(crate::tools::SkillsListTool::new(Arc::clone(skills))));

+ // SkillManageTool — CRUD for skills.
+ tools.register(Arc::new(crate::tools::SkillManageTool::new(
+     Arc::clone(skills),
+     workspace_dir.to_path_buf(),
+ )));
```

### 9.2 cmd_exec.rs / cmd_chat.rs

同步注册 `SkillsListTool` 和 `SkillManageTool`。

### 9.3 tools/mod.rs + tools/lib.rs

```rust
mod skill_tool;
+ mod skills_list_tool;
+ mod skill_manage_tool;

pub use skill_tool::SkillTool;
+ pub use skills_list_tool::SkillsListTool;
+ pub use skill_manage_tool::SkillManageTool;
```

---

## 十、完整文件清单

### 新增文件

| 文件 | 内容 |
|---|---|
| `src/tools/skills_list_tool.rs` | `SkillsListTool` — 只读目录查询 |
| `src/tools/skill_manage_tool.rs` | `SkillManageTool` — 6 action CRUD |

### 修改文件

| 文件 | 改动摘要 |
|---|---|
| `src/str_utils.rs` | +`extract_yaml_bool()` |
| `src/agents/workspace/skill_loader.rs` | `SkillDefinition` +6 字段 + 解析逻辑 + 测试 |
| `src/agents/workspace/skills.rs` | `Skill` +7 字段 (含 skill_dir) + `from_definition` 映射 + `agent_skills_iter()` + `skills_iter()` + `reload()` |
| `src/tools/skill_tool.rs` | name → `skill_view` + `agent_invocable` 校验 + `file_path` 参数 |
| `src/tools/mod.rs` | +2 mod + 2 pub use |
| `src/tools/lib.rs` | +2 mod + 2 pub use |
| `src/agents/attachment.rs` | `diff_skills` 用 `agent_skills_iter()` + `render_skills` 增强 + `"use_skill"` → `"skill_view"` |
| `src/agents/slash_command.rs` | `/skills` 展示 version + invocable 标注 |
| `src/daemon.rs` | `build_tools()` 注册 2 个新工具 |
| `src/cli/cmd_exec.rs` | 注册 2 个新工具 |
| `src/cli/cmd_chat.rs` | 注册 2 个新工具 |

---

## 十一、风险与取舍

1. **不做 fuzzy match**：Hermes 的 patch 用 fuzzy_match 容忍空格差异。MyClaw 先用精确匹配（和 `file_edit` 工具一致），如果模型频繁匹配失败再考虑加
2. **不做安全扫描**：Hermes 有 `skills_guard` 扫描恶意代码。MyClaw 的 skill 目录在用户自己的 workspace 下，信任模型操作
3. **不做 pin 保护**：Hermes 有 pinned skill 机制防误删。MyClaw 只保护 `self` skill
4. **原子写入简化**：先写 `.tmp` 再 `rename`，不做 Hermes 那样的 `tempfile::mkstemp`，够用
5. **arguments 占位符暂不实现**：`arguments` 字段先存着，未来做 `$arg_name` 替换时再用
6. **不做 absorbed_into 声明**：Hermes 删除 skill 时支持 `absorbed_into` 参数声明合并/修剪意图。MyClaw 暂不需要后台 curator，删除就是删除
7. **不做 replace_all**：patch 只做单次精确替换。和 `file_edit` 工具保持一致
8. **不做 category 子目录**：create 直接在 `skills/{name}/` 下，不平铺到 `skills/{category}/{name}/`。未来如需分组再加 `category` 参数
9. **不做 readiness 环境检查**：Hermes 的 `required_environment_variables` + setup_needed 机制用于检测 API Key 等前置条件。MyClaw 暂不需要

---

## 十二、实现顺序

依赖链自底向上，建议按此顺序开发：

```
第 1 层（无依赖）
  str_utils.rs — extract_yaml_bool

第 2 层（依赖 str_utils）
  skill_loader.rs — SkillDefinition +6 字段 + parse 逻辑 + 测试

第 3 层（依赖 skill_loader）
  skills.rs — Skill +7 字段 + from_definition + agent_skills_iter + reload + skills_iter

第 4 层（依赖 skills，互不依赖，可并行）
  skill_tool.rs      — skill_view 改造
  skills_list_tool.rs — 新增
  skill_manage_tool.rs — 新增

第 5 层（消费 skills + 注册工具）
  attachment.rs     — render_skills 增强
  slash_command.rs  — /skills 增强
  daemon.rs         — 注册 3 个工具
  cmd_exec.rs       — 注册 3 个工具
  cmd_chat.rs       — 注册 3 个工具
  tools/mod.rs      — mod + pub use
  tools/lib.rs      — mod + pub use
```

每个层级完成后编译验证，确保不破坏现有功能。第 4 层的三个工具文件相互独立，可以分别实现和测试。

---

## 十三、测试策略

| 模块 | 测试重点 | 说明 |
|---|---|---|
| `str_utils::extract_yaml_bool` | `true`/`false`/`yes`/`no`/无效值/缺失 key | 单测，覆盖所有分支 |
| `skill_loader::parse_skill_file` | 6 个新字段解析 + 缺失字段默认值 + 连字符/下划线兼容 | 单测，用临时 .md 文件 |
| `Skill::from_definition` | 字段映射完整性 + `skill_dir` 从 `source_path` 提取 | 单测 |
| `SkillManager` | `reload` 覆盖 + `agent_skills_iter` 过滤 + `skills_iter` 全量 | 单测 |
| `scan_skill_files` | 空目录 + 部分子目录 + 混合文件类型 | 单测，用临时目录 |
| `SkillManageTool` | 6 个 action 端到端 + 参数校验 + 路径穿越防护 + 内置 skill 保护 | 集成测试，写到临时 `workspace_dir` |
| `SkillsListTool` | 空 skill 目录 + 多 skill + 字段裁剪（false 字段不返回） | 集成测试 |
| `SkillTool` | 主文件加载 + 辅助文件加载 + 二进制文件 + 路径穿越 + `agent_invocable` 拒绝 | 集成测试 |
| `render_skills` | 新增 when_to_use/argument_hint 展示 + `agent_invocable=false` 过滤 | 单测 |

**关键边界用例**：

```
- skill_manage(create, name="self") → 拒绝
- skill_manage(patch, old_string="不存在的内容") → 返回文件预览
- skill_view(name="xxx", file_path="../../etc/passwd") → 路径穿越拒绝
- skill_view(name="xxx", file_path="scripts/icon.png") → 二进制文件兜底
- skill_manage(create) 前后检查 SkillManager 是否刷新
- 同名 skill 重复 create → 第二次报错
- frontmatter name 与参数 name 不一致 → 报错
```
