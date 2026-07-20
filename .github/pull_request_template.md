# 变更说明

<!-- 这个 PR 解决什么问题？为什么这样做？关联 issue 请写 Closes #123 -->

## 变更类型

- [ ] feat（新功能）
- [ ] fix（缺陷修复）
- [ ] docs / ci / refactor / test / chore

## 自测清单

- [ ] `npm test -- --run` 通过
- [ ] `npm run lint` 通过
- [ ] `npm run build` 通过
- [ ] `cargo test --manifest-path src-tauri/Cargo.toml --all-targets --all-features` 通过（涉及 Rust 时）
- [ ] `cargo fmt --check` 与 `cargo clippy -- -D warnings` 通过（涉及 Rust 时）
- [ ] `npm --prefix deploy/coordinator test` 通过（涉及协调服务时）
- [ ] commit 已使用 `git commit -s` 附带 DCO 署名

## 边界确认

- [ ] 凭据仍只保存在车主本机，未新增任何非官方外呼地址
- [ ] 普通页面未出现 P2P、WebRTC、TURN、密钥等技术术语
- [ ] 未引入押金、积分、结算等交易功能
