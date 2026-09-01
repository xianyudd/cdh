## 改了什么 / What

<!-- 用户能感知到的变化，一两句话说清 -->

## 为什么 / Why

<!-- 触发这次改动的问题或需求。有对应 issue 就链接（Closes #123） -->

## 验证 / Verification

<!-- 贴实际跑过的命令和结论。没跑的就写「没跑」，不要留空勾 -->

- [ ] `just fmt-check`
- [ ] `just shell-lint`
- [ ] `just lint-strict`（CI 跑的是这条，`just check` 里的 `lint` 不带 `-D warnings`）
- [ ] `just test`

改到下面这些地方时，额外说明：

- [ ] 动了 `controller` / `picker` / shell payload —— 补了测试，或写明手动验证步骤
- [ ] 动了安装器 —— 说明影响 bash / fish / zsh 中的哪几个，以及卸载路径是否受影响
- [ ] 动了依赖或 `Cargo.lock` —— 确认 MSRV（`rust-version`）下仍可编译

## 备注 / Notes

<!-- 已知遗留、后续计划、需要 reviewer 特别看的地方 -->

<!--
提交信息规范见 CONTRIBUTING.md：<type>(<scope>): <summary>，
scope 取值有白名单，标题不超过 72 字符且结尾不加标点。
本地可用 `just hooks-install` 装上 commit-msg 校验。
-->
