# 发布、签名与自动更新

本文档是把「CI 产物」升级为「可信正式分发」的操作清单。当前状态：安装包未签名/未公证，自动更新未启用；每一节完成后请更新本文档顶部的状态表。

| 项 | 状态 |
| --- | --- |
| macOS Developer ID 签名 + 公证 | 未完成 |
| Windows 代码签名 | 未完成 |
| Linux 校验（SHA256SUMS） | 已有（Release 附带） |
| 自动更新（tauri-plugin-updater） | 未启用，激活步骤见下 |

## 1. 常规发布流程

1. 同步更新三处版本号：`package.json`、`src-tauri/tauri.conf.json`、`src-tauri/Cargo.toml`。
2. 在 `CHANGELOG.md` 把 `[Unreleased]` 内容归档为 `[X.Y.Z] - 日期`。
3. 提交后打 tag：`git tag vX.Y.Z && git push origin vX.Y.Z`。
4. CI（`build-desktop.yml`）会校验 tag 与应用版本一致，构建全部安装包并创建 GitHub Release 附带 `SHA256SUMS.txt`。

## 2. macOS：Developer ID 签名与公证

需要 [Apple Developer Program](https://developer.apple.com/programs/)（99 USD/年）。

1. 在开发者账号创建 **Developer ID Application** 证书，导出为 `.p12`。
2. 在 App Store Connect 生成公证用 **API Key**（或使用 Apple ID + app-specific password）。
3. 在 GitHub 仓库 Secrets 配置：
   - `APPLE_CERTIFICATE`（`.p12` 的 base64）、`APPLE_CERTIFICATE_PASSWORD`
   - `APPLE_SIGNING_IDENTITY`（如 `Developer ID Application: Your Name (TEAMID)`）
   - 公证三选一：`APPLE_API_ISSUER` + `APPLE_API_KEY` + `APPLE_API_KEY_PATH`，或 `APPLE_ID` + `APPLE_PASSWORD` + `APPLE_TEAM_ID`
4. 在 `build-desktop.yml` 的 macOS 矩阵项把上述 Secrets 透传为环境变量（Tauri 检测到即自动签名并公证）：

```yaml
      - name: Build installer
        run: npm run tauri build -- ${{ matrix.build_args }}
        env:
          APPLE_CERTIFICATE: ${{ secrets.APPLE_CERTIFICATE }}
          APPLE_CERTIFICATE_PASSWORD: ${{ secrets.APPLE_CERTIFICATE_PASSWORD }}
          APPLE_SIGNING_IDENTITY: ${{ secrets.APPLE_SIGNING_IDENTITY }}
          APPLE_ID: ${{ secrets.APPLE_ID }}
          APPLE_PASSWORD: ${{ secrets.APPLE_PASSWORD }}
          APPLE_TEAM_ID: ${{ secrets.APPLE_TEAM_ID }}
```

5. 验证：`spctl -a -t open --context context:primary-signature -v 可信拼车.dmg`。

## 3. Windows：代码签名

三条路线按性价比排序：

1. **Azure Trusted Signing**：按月订阅，适合个人/小团队；Tauri 2 原生支持（`bundle.windows.signCommand` 或社区 action）。
2. **SignPath.io 开源计划**：对满足条件的开源项目免费，通过其 GitHub App 在 CI 内签名。
3. **传统 OV/EV 证书**（Certum、SSL.com 等）：把证书指纹配置到 `tauri.conf.json` 的 `bundle.windows.certificateThumbprint`，并在 CI 用 `signtool`。

无论哪种，都在 `build-desktop.yml` 的 Windows 矩阵项接入，产物是已签名的 NSIS 安装器。验证：`signtool verify /pa /v 安装包.exe`。

## 4. 自动更新（tauri-plugin-updater）激活清单

> 前置条件：macOS/Windows 签名先就绪，否则更新包会再次触发系统拦截。更新签名密钥独立于操作系统签名证书，可提前生成。

1. 生成更新签名密钥（私钥务必离线保存 + 存入 CI Secret，公钥进仓库配置）：

```bash
npm run tauri signer generate -- -w ~/.tauri/trusted-carpool.key
```

2. 安装依赖：

```bash
npm run tauri add updater        # 同时写入 Cargo.toml 与 capabilities
npm i @tauri-apps/plugin-updater
```

3. `src-tauri/tauri.conf.json` 增加：

```json
{
  "bundle": { "createUpdaterArtifacts": true },
  "plugins": {
    "updater": {
      "pubkey": "<signer generate 输出的公钥>",
      "endpoints": [
        "https://github.com/sunjackson/ai-trusted-carpool/releases/latest/download/latest.json"
      ]
    }
  }
}
```

4. CI 构建环境注入 `TAURI_SIGNING_PRIVATE_KEY` 与 `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` Secrets；release job 生成并上传 `latest.json`（包含各平台更新包 URL 与签名），推荐直接改用 `tauri-apps/tauri-action`，它会自动生成。
5. 前端在启动后台静默检查更新（`check()` → 提示 → `downloadAndInstall()`），交互遵循「每页一个主动作」，只在托盘/关于页提示。
6. 发布一个旧版本 → 新版本的真实升级演练后，把本文档状态表更新为「已启用」。

## 5. 发布前检查清单

- [ ] 版本号三处一致，CHANGELOG 归档
- [ ] `npm test -- --run`、`npm run lint`、coordinator 测试、`cargo test/fmt/clippy` 全绿
- [ ] `SHA256SUMS.txt` 与所有安装包一起附在 Release
- [ ] Release Notes 提及 LEGAL.md 使用须知
- [ ] （启用签名后）macOS `spctl` 验证、Windows `signtool verify` 通过
- [ ] （启用更新后）`latest.json` 指向的所有资产可下载、签名可验证
