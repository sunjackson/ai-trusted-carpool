# 贡献指南

感谢你愿意为「可信拼车」贡献代码、文档或想法。开始之前请先阅读 [LEGAL.md](LEGAL.md) 了解项目的使用边界。

## 开发环境

| 依赖 | 版本 |
| --- | --- |
| Node.js | >= 20 |
| Rust（stable）| 随 `rustup` 安装，含 `rustfmt`、`clippy` |
| 平台依赖 | Linux 需要 `libwebkit2gtk-4.1-dev`、`libayatana-appindicator3-dev`、`librsvg2-dev`、`patchelf`、`xdg-utils` |

```bash
npm ci
npm run dev                 # React/Vite 前端（浏览器 demo 模式）
npm run tauri dev           # 完整桌面端
```

## 提交前自测

请确保以下命令全部通过（CI 会以同样标准阻塞合并）：

```bash
npm test -- --run                                                        # 前端 Vitest
npm run lint                                                             # ESLint（--max-warnings 0）
npm run build                                                            # tsc + vite build
npm --prefix deploy/coordinator test                                     # 协调服务 node:test
cargo fmt --manifest-path src-tauri/Cargo.toml --check                   # Rust 格式
cargo test --manifest-path src-tauri/Cargo.toml --all-targets --all-features
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets --all-features -- -D warnings
```

## Commit 规范

沿用仓库现有的 Conventional Commits 风格，小写类型前缀 + 一句话描述「为什么」：

```
feat: make friend invites one-click and keep ride status visible
fix: keep Codex desktop launch working after the app rename
ci: make native installer delivery reproducible across platforms
docs: ...
refactor: ...
test: ...
```

## DCO 署名

本项目使用 [Developer Certificate of Origin](https://developercertificate.org/)。每个 commit 请使用 `git commit -s` 追加署名行：

```
Signed-off-by: Your Name <you@example.com>
```

提交即表示你有权以 Apache-2.0 许可证贡献这些内容。

## PR 流程

1. Fork 并从 `main` 拉出特性分支（`feat/xxx`、`fix/xxx`）。
2. 保持 PR 聚焦单一主题；行为变更需附带测试。
3. 填写 PR 模板中的变更说明与自测清单。
4. CI 全绿后等待维护者 review；讨论遵循 [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md)。

## 设计与产品约束

- 普通页面不出现 P2P、WebRTC、TURN、密钥、会话、审计等技术术语（详见 [docs/PRODUCT-BRIEF.md](docs/PRODUCT-BRIEF.md)）。
- 每个页面只有一个主要动作。
- 凭据永远不离开车主本机；客户端只允许连接 Anthropic、OpenAI/ChatGPT 官方接口。
- 价格口径只使用官方公开标准价，未知模型显示「暂无官价」，绝不套用相近模型价格（详见 [docs/PRICING-SOURCES.md](docs/PRICING-SOURCES.md)）。

## Good first issues：界面文案国际化

前端已内置最小 i18n 骨架（`src/i18n.ts`，默认 `zh-CN`，可选 `en`），目前只迁移了首启确认页与欢迎页作为示范。以下工作适合首次贡献者认领，欢迎按页面拆分提交：

- 将 `src/App.tsx` 中其余页面（发车设置、发车中、上车、选择工具、使用中）的中文文案迁移到 `src/i18n.ts` 字典；
- 补全对应的英文翻译；
- 为语言偏好增加一个折叠的切换入口（保存到 `localStorage` 的 `trusted-carpool:locale`）；
- 翻译 Rust 端用户可见的错误信息。

认领时请在 issue 中注明页面范围，避免重复劳动。
