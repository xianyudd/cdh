# Commit message convention

```text
<type>(<scope>): <summary>
```

## Types

- `feat`, `fix`, `docs`, `refactor`, `test`, `chore`, `ci`, `perf`

## Preferred scopes

- `install`, `bash`, `fish`, `zsh`, `history`, `paths`, `recommend`, `controller`, `readme`, `release`, `ci`, `tui`

## Rules

- Subject（标题行）一律英文，且只用可打印 ASCII——type/scope 本就是英文，主体混入中文会让 git log --grep 与 GitHub 检索失焦；由 commit-msg 钩子强制（2026-09-03 起，历史中文标题不改写）
- Body（正文）语言不限，中文详述 encouraged
- `summary` 保持简洁、动作导向
- 每条提交聚焦一个主要变更
- 只有在 scope 能增加信息时才填写
