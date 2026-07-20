# 更新日志

本文件记录项目的所有重要变更，格式遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/)。

## [Unreleased]

### Added

- Claude Code / Codex 命令行一键安装（参考 CC Switch）：发车页、乘客选择工具页与使用中页面在检测到未安装时提供安装按钮，调用官方 npm 包并给出权限/网络失败的可读提示与手动命令；安装中显示已用时长。
- 工具检测增强：npm 安装的 CLI 显示版本号（读取全局包 `package.json`，不拖慢检测）；未安装 Node.js 时提前禁用一键安装并给出指引。
- 开源基础设施：Apache-2.0 许可证、NOTICE、贡献指南、行为准则、安全政策、issue/PR 模板与 Dependabot 配置。
- 使用须知与免责声明（`LEGAL.md`），应用首次启动时的一次性风险确认页。
- Open Core 商业模式说明（`docs/BUSINESS-MODEL.md`）、自建部署指南（`docs/SELF-HOSTING.md`）与发布签名清单（`docs/RELEASE.md`）。
- 协调服务参考实现新增 `/api/v1/turn-credentials` 接口（coturn REST API 时效凭据方案），自建 TURN 中继可用。
- 客户端的 TURN 域名校验与官方上车链接域名改为从已配置的协调服务地址派生，自建部署无需改代码（默认仍为 `p2p.cnaigc.ai`）。
- 前端最小 i18n 骨架（默认 `zh-CN`，可选 `en`），首启确认页与欢迎页已迁移。
- CI 安全加固：`cargo audit` 与 CodeQL（JavaScript/TypeScript + Rust）扫描；Vitest 覆盖率报告。

### 初始开发（2026-07，尚未发布正式版本）

- Claude Code 与 Codex 的一键发车/一键上车核心流程：签名邀请、设备绑定、四座并发。
- WebRTC 直连优先、TURN 兜底的端到端加密转发，本地 HTTP 代理流式透传官方响应。
- 按成员 → 工具 → 模型的实时用量统计与官方 USD 标准价估算；成员 5 小时/24 小时/7 天滚动 Token 限额。
- 车主官方 Claude/Codex OAuth 账号剩余额度同步展示；API Key 无额度接口时明确显示不可用。
- 官方桌面客户端安全接入（配置写入前完整备份、离车/退出/下次启动时恢复）。
- macOS 菜单栏、Windows 托盘与 Linux 状态区常驻状态；关闭主窗口后驻留后台。
- 本地追加式用量历史（不含提示词、响应、密钥）；一键上车官方链接与深链解析的严格校验。
- GitHub Actions 多平台安装包构建（macOS 通用 DMG、Windows NSIS、Linux DEB/AppImage）与打 tag 自动发布。
