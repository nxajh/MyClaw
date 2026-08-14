# Worktree 委托说明

本说明描述 `agent_delegate` 工具通过 git worktree 隔离子代理工作的机制。

## workspace 参数

- `agent_delegate` 工具支持 `workspace` 参数：指定子代理工作区（git 仓库根目录）。
- `isolation=worktree` 的子代理（如 coder）：必须传 `workspace`，否则委托报错。

## 工作流程

1. coordinator 在 `workspace` 指定的仓库创建 worktree。
2. 子代理在 worktree 中工作并 commit。
3. 完成后 coordinator 自动 merge 回 `workspace` 仓库主分支并清理 worktree。

## 分支命名

- 子代理 worktree 分支命名规则：`subagent/{agent_name}_{8位hex}`。
- 子代理必须在其注入的 worktree 工作区内操作（相对路径基准），不得直接修改主仓库
