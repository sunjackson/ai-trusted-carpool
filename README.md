# 可信拼车

面向熟人之间的 Claude Code 与 Codex 本机账号共享桌面端。车主选择一个明确的开放时间段，一键发车并获得四个座位码；乘客输入座位码后即可打开任一工具，两个工具可同时运行。

## 核心能力

- Claude Code 与 Codex 平等支持，可单独或同时发车。
- 每辆车最多四名乘客并发，每个座位独立绑定设备。
- 优先 WebRTC 直连，失败时自动使用 TURN；应用层请求仍端到端加密。
- 密钥只保存在车主本机，只允许 Anthropic、OpenAI/ChatGPT 官方接口。
- 按成员 → 工具 → 模型实时统计请求、输入、输出、缓存读写及官方 USD 标准价估算。
- 本地追加式历史只记录用量元数据，不保存提示词、响应正文、密钥、会话密钥或上车码。

## 本地开发

```bash
npm ci
npm run dev                 # React/Vite 前端
npm run tauri dev           # 桌面端
npm test -- --run           # Vitest
npm run lint
cargo test --manifest-path src-tauri/Cargo.toml --all-targets --all-features
```

## 打包

```bash
./scripts/build-macos-universal.sh
./scripts/build-windows-cross.sh
./scripts/build-linux-docker.sh
```

GitHub Actions 会在 macOS、Windows 与 Ubuntu 原生环境中执行完整检查并生成安装包。macOS 正式分发仍需 Apple Developer ID 与公证；Windows 正式分发仍需代码签名证书。

## 安全边界

12 字符上车码约有 60-bit 随机熵，并受服务端限速和发车时段约束；它只负责查找签名邀请。成功认领后使用独立 256-bit 会话密钥，座位授权同时绑定乘客设备身份。产品前期仅面向已认识的人，不包含押金、积分或结算功能。

架构、安全与价格口径见 [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)、[`docs/PRODUCT-BRIEF.md`](docs/PRODUCT-BRIEF.md) 和 [`docs/PRICING-SOURCES.md`](docs/PRICING-SOURCES.md)。当前 UI 基准见 [`design/ui-design-board-v4.png`](design/ui-design-board-v4.png)。
