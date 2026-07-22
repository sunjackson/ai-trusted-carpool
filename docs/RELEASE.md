# 发布、签名与自动更新

本文档是把「CI 产物」升级为「可信正式分发」的操作清单。v0.0.5 已完成签名更新代码与 fail-closed 发布流水线；仓库 Secrets 和平台证书仍是正式发布的外部前置条件。

## 0. 未签名测试预发布

测试阶段可以推送 `vX.Y.Z-test.N` 标签（`N` 从 1 开始）生成 GitHub **Pre-release**。该通道只用于用户主动下载和自行部署：

- 三处应用版本仍必须等于 `X.Y.Z`，标签必须精确匹配 `vX.Y.Z-test.N`；
- CI 仍执行完整源码验证与三平台打包，但不读取任何签名 Secret；
- 发布构建使用 `tauri.distribution.conf.json` 生成 GitHub 不会改写的 ASCII 资产文件名；应用窗口与界面中的中文名称不受影响；
- Release 只包含 macOS DMG、Windows NSIS、Linux DEB/AppImage 与 `SHA256SUMS.txt`；
- CI 会拒绝测试预发布中出现 `.sig` 或 `latest.json`，因此它不会进入应用内自动更新；
- Windows/macOS 会显示未知发布者或未公证警告。Release Notes 必须明确说明风险，不能将测试包称为正式或可信签名版本。

正式发布仍使用精确的 `vX.Y.Z` 标签，并继续强制执行后文的全部签名门禁。不得用测试标签替代正式发布。

| 项 | 状态 |
| --- | --- |
| macOS Developer ID 签名 + 公证 | 未完成 |
| Windows 代码签名 | CI 已接入 PFX、固定指纹与 `signtool verify`；待仓库 Secrets |
| Linux 校验（SHA256SUMS） | 已有（Release 附带） |
| Tauri 更新签名 | 独立公钥已内置；tag 构建强制要求 CI 私钥 |
| 自动更新 | Windows / Linux AppImage 已实现；macOS / DEB 保持手动 |

## 1. 常规发布流程

1. 同步更新三处版本号：`package.json`、`src-tauri/tauri.conf.json`、`src-tauri/Cargo.toml`。
2. 在 `CHANGELOG.md` 把 `[Unreleased]` 内容归档为 `[X.Y.Z] - 日期`。
3. 在 GitHub 仓库创建针对 `refs/tags/v*` 的 tag ruleset：限制 tag 创建者，禁止非管理员更新/删除发布 tag。Environment 的 deployment filter 不会保护 tag 的创建，不能替代该 ruleset。
4. 创建受保护的 GitHub Environment `release`，只允许 ruleset 保护的 `v*` tag 部署并配置 Required reviewers；把第 3、4 节列出的签名 Secrets 只放在该 Environment，不能保留同名仓库级 Secrets。
5. 提交进入 `main` 后再打 tag：`git tag vX.Y.Z && git push origin vX.Y.Z`。CI 会在进入签名矩阵前校验 tag、三处版本号、双语 Release Notes 以及提交可从 `origin/main` 到达；签名后再用内置公钥对每个更新产物做密码学验签，最后生成指向已签名产物的 `latest.json`、`SHA256SUMS.txt` 与 GitHub Release。测试预发布则使用 `vX.Y.Z-test.N`，并按第 0 节隔离。

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

当前 workflow 使用已有 PFX 证书。仓库管理员必须在受保护的 `release` Environment 中配置：

- `WINDOWS_CERTIFICATE_PFX`：PFX 的 base64 内容；
- `WINDOWS_CERTIFICATE_PASSWORD`：PFX 口令；
- `WINDOWS_CERTIFICATE_THUMBPRINT`：预期 SHA-1 指纹（去空格、不区分大小写）。

tag 构建会把证书临时导入当前用户证书库，只选择带私钥的证书，比较实际与固定指纹，再把指纹写入临时 Tauri overlay。构建后同时读取安装器的 Authenticode signer 指纹并与固定值比较，再执行 `signtool verify /pa /all /v`；任一 Secret 缺失、指纹不符或验证失败都会阻止发布。PFX 与口令不得写入仓库或 Actions Artifact。

若未来更换签名服务，可在独立提交中选择以下路线：

1. **Azure Trusted Signing**：按月订阅，适合个人/小团队；Tauri 2 原生支持（`bundle.windows.signCommand` 或社区 action）。
2. **SignPath.io 开源计划**：对满足条件的开源项目免费，通过其 GitHub App 在 CI 内签名。
3. **传统 OV/EV 证书**（当前路径）：CI 临时导入 PFX，以 Secret 中固定的证书指纹签名并验证。

普通分支 / PR 的 Actions Artifact 仍是不带系统代码签名的开发产物；只有通过上述 tag 门禁的 Release 才可称为正式 Windows 安装包。

## 4. 签名自动更新

更新签名与 Windows/macOS 系统代码签名是两套独立密钥。应用只接受 `tauri.conf.json` 内置公钥验证通过的更新包；签名校验不能关闭。

1. 本项目已生成独立 Tauri 更新密钥。私钥只允许离线保存并写入受保护 `release` Environment 的 Secret `TAURI_SIGNING_PRIVATE_KEY`；如私钥有口令，再配置 `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`。**不得把私钥、口令或其内容写入仓库级 Secret、issue、日志、仓库与诊断包。**

   当前 `tauri.conf.json` 中 `plugins.updater.pubkey` **base64 配置字符串**的 SHA-256：`e773d48d10f364b1f38b827109e7c4a2f5203e61561ec89bd1e2da94b1e7170d`。这不是解码后的 `.pub` 文件哈希；轮换校验必须对配置字符串本身计算。

   Tauri updater 当前只接受一把公钥，不能同时信任旧、新两把 key。轮换时必须先用**旧私钥**签出一个内置**新公钥**的桥接版本，确认足够多客户端已安装桥接版本后，后续版本才改用新私钥。错过桥接版本的客户端需要版本感知更新端点继续提供桥接包，或改为手动安装；不要直接让已安装版本失去升级路径。

2. release 构建使用独立 overlay `src-tauri/tauri.release.conf.json` 开启 `bundle.createUpdaterArtifacts`。基础配置不打开该项，因此没有私钥的本地与 PR 构建不会意外失败。

3. `scripts/verify-updater-artifacts.mjs` 会使用 `tauri.conf.json` 中的内置公钥逐一验证产物内容、Minisign 全局签名及签名内文件名，并固定检查 `pubkey` base64 配置字符串的 SHA-256；错误 CI 私钥、篡改产物或签名/文件名错配都会阻止发布。随后 `scripts/generate-updater-manifest.mjs` 只接受一组 Windows x86_64 NSIS + `.sig` 和一组 Linux x86_64 AppImage + `.sig`，缺失、重复、空签名、架构、版本/tag 不一致均 fail closed。`latest.json` 只是指向已签名产物的更新元数据，本身没有独立签名，并明确排除：

   - macOS：取得 Developer ID、完成公证并做旧版到新版真实升级演练前，不发布自动安装目标；
   - Linux DEB：继续交由发行版包管理器或 Release 页面手动安装。

4. 客户端流程为“检查 → 显示进度并下载 → 签名验证 → 用户显式安装并重启”。发车或上车活跃期间可以下载，但 Rust 后端会拒绝安装；验证后的下载仍保留为 pending。签名、下载或安装失败都保留当前版本并提供固定官方 Release 页面回退。

5. GitHub Release 必须同时包含 Windows/Linux 更新包与 `.sig`、`latest.json`、所有安装包和 `SHA256SUMS.txt`，并使用 `docs/releases/vX.Y.Z.md` 的中英双语说明。CI 先创建 draft，上传后逐项核对远端文件名与字节数，全部一致才一次性公开；任一上传失败只会留下未公开 draft。

6. Tauri 的产物签名不直接覆盖 `latest.json`。为防止发布通道交换平台/架构，或把旧的合法签名产物伪装成更高版本回放，客户端会先用 Minisign 全局签名认证 trusted comment 中的文件名，再要求它与下载 URL、当前平台/架构的精确产物后缀以及声明的完整 SemVer 一致；稳定版不会接受同基础版本的 prerelease/build 文件。下载完成后 Tauri 再验证产物内容签名。`latest.json` 中的说明、日期等非执行元数据仍可被发布通道篡改或隐藏，因此界面不把这些字段作为安全决策依据。

重新生成密钥（仅轮换演练时使用）：

```bash
npm run tauri signer generate -- -w ~/.tauri/trusted-carpool-updater.key
```

## 5. 发布前检查清单

- [ ] 版本号三处一致，CHANGELOG 归档
- [ ] 仓库 `refs/tags/v*` ruleset 限制创建/更新/删除；`release` Environment 启用 Required reviewers，且签名 Secrets 不存在仓库级副本
- [ ] `npm test -- --run`、`npm run test:release`、`npm run lint`、coordinator 测试、`cargo test/fmt/clippy/audit` 全绿
- [ ] WebRTC direct、TURN UDP、TURN TCP 与三平台打包通过
- [ ] `SHA256SUMS.txt` 与所有安装包一起附在 Release
- [ ] Release Notes 提及 LEGAL.md 使用须知
- [ ] macOS 仍为手动更新；启用前完成 `codesign`、`spctl`、公证和真实升级演练
- [ ] Windows `signtool verify /pa /all /v` 通过
- [ ] 内置公钥对下载后的全部更新产物验签通过；`latest.json` 只含 Windows / Linux AppImage，全部 URL 可下载
