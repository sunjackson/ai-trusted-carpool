# Trusted Carpool

[中文](README.md) | **English**

[![CI](https://github.com/sunjackson/ai-trusted-carpool/actions/workflows/build-desktop.yml/badge.svg)](https://github.com/sunjackson/ai-trusted-carpool/actions/workflows/build-desktop.yml)
[![CodeQL](https://github.com/sunjackson/ai-trusted-carpool/actions/workflows/codeql.yml/badge.svg)](https://github.com/sunjackson/ai-trusted-carpool/actions/workflows/codeql.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Release](https://img.shields.io/github/v/release/sunjackson/ai-trusted-carpool?include_prereleases)](https://github.com/sunjackson/ai-trusted-carpool/releases)

> [!WARNING]
> **Before you use this**: sharing one official Claude/Codex subscription account among several people directly conflicts with Anthropic's Consumer Terms and OpenAI's Terms of Use, which prohibit sharing account credentials or making your account available to anyone else. Your account may be rate-limited or permanently banned without a refund. Use it only with people you personally trust; the account owner bears all risk. See [LEGAL.md](LEGAL.md).

A desktop app for sharing a locally signed-in Claude Code / Codex account among people who already know and trust each other. The host can choose a fixed window or all-day hosting and gets four seat codes; a passenger enters one code and can open either tool — both can run at the same time. After a reboot, the host can restore the original channel and codes or discard it and start a new car. **The code and self-hosting are free and open source forever** — see the [business model](docs/BUSINESS-MODEL.md).

![UI design board](design/ui-design-board-v4.png)

## Table of contents

- [Highlights](#highlights)
- [Install](#install)
- [Update and release trust](#update-and-release-trust)
- [Local development](#local-development)
- [Packaging](#packaging)
- [Security boundary](#security-boundary)
- [Self-hosting](#self-hosting)
- [How the project sustains itself](#how-the-project-sustains-itself)
- [Contributing](#contributing)
- [License](#license)

## Highlights

- Claude Code and Codex are supported as equals; host either or both.
- Zero-setup passengers: installing this app is enough. A missing CLI is fetched as the official standalone binary (Claude Code via the `downloads.claude.ai` manifest, Codex via the GitHub `openai/codex` releases), SHA-256-verified before activation, with live progress and cancel — **no Node.js, no admin rights, and no personal AI account required**. npm install remains a fallback when the official channel is unreachable and Node.js exists.
- App-managed CLIs get one-click updates (official release metadata cached in the background, older versions pruned); a user's own system install always wins.
- The host copies an official `https://p2p.cnaigc.ai/api/v1/carpool/join/<code>` link; a friend clicks it, the client launches with the seat pre-filled, and a saved nickname makes joining one click.
- After joining, passengers open the Claude/Codex terminal or the official desktop client with one click; the desktop client is preferred when installed.
- Up to four concurrent passengers per car; every seat is bound to one device.
- Fixed-window and all-day hosting are both supported. All-day invites remain discoverable only while the host is online: the app renews short coordinator leases, so shutdowns and network loss naturally make the car offline.
- After a restart, the host chooses between restoring the original channel or starting a new car. A restore keeps the car ID and four codes, but passengers must claim again and receive fresh session authorization.
- The home page keeps separate host and passenger history. Click an entry to inspect its mode, time window, tools, seat, and nickname. Passenger entries show online, scheduled, offline, or expired status and enable rejoin only while the car is available.
- WebRTC direct connection first, automatic TURN fallback; both application payloads and connection signaling are end-to-end encrypted (the coordinator never sees SDP or candidate IPs).
- Automatic drop recovery: passenger-side heartbeat detection plus exponential-backoff reconnects (with fresh TURN credentials), and the ride page shows the real link state.
- Credentials never leave the host machine; only official Anthropic and OpenAI/ChatGPT endpoints are allowed.
- Real-time per-member → per-tool → per-model stats: requests, input, output, cache read/write, and official USD list-price estimates.
- The member list stays concise (totals, requests, price, key limits); click a member for the per-model breakdown.
- The host can set independent rolling 5-hour / 24-hour / 7-day token limits per member.
- Host and online members see the same official Claude/Codex subscription quota; API-key auth shows "unsupported" instead of invented numbers.
- The append-only `usage-history.jsonl` stores usage metadata only — never prompts, response bodies, credentials, session secrets, or seat codes. A separate private `ride-history.json` stores only the car summary and seat codes required for history/recovery, never access/session secrets, device private keys, WebRTC signaling, or request bodies; writes are atomic, corrupt files are quarantined, and Unix permissions are `0600`.
- The macOS menu bar, Windows tray, and Linux status area mirror idle/hosting/riding state; closing the main window keeps the app resident.

## Install

Download the installer for your platform from [GitHub Releases](https://github.com/sunjackson/ai-trusted-carpool/releases) (macOS universal DMG, Windows x64 NSIS, Linux x64 DEB/AppImage), then compare its SHA-256 digest with `SHA256SUMS.txt` from the same release. The currently recommended download is the unsigned stable [v0.0.6 release](https://github.com/sunjackson/ai-trusted-carpool/releases/tag/v0.0.6), which includes fixes for the first-launch blank window on Windows, passenger connectivity, and ride-history details. See the complete [release notes](docs/releases/v0.0.6.md).

## Current unsigned release policy

> [!IMPORTANT]
> The project currently has no Windows Authenticode certificate/PFX, Apple Developer ID, notarization credentials, or Tauri updater signing key. GitHub Actions builds these **unsigned manual-distribution packages** from the public source, so Windows and macOS security warnings are expected and do not mean the package has passed operating-system signature verification.

| Package | Current update path | Expected installation warning |
| --- | --- | --- |
| Windows x64 NSIS | Manual download and install from GitHub Releases | SmartScreen “Windows protected your PC,” low-reputation download, or UAC “Unknown publisher” |
| macOS universal DMG | Manual download from GitHub Releases, then drag to Applications | Gatekeeper cannot verify the developer or check the app for malicious software |
| Linux x64 AppImage | Manual download, checksum verification, and executable permission | The file manager may report that the file is not executable or comes from an unknown source |
| Linux x64 DEB | Manual installation through the system package manager | The software center may identify it as a third-party or out-of-repository package |

- Exact `vX.Y.Z` tags now publish stable GitHub Releases, but their installers remain unsigned manual downloads. “Stable” describes the version channel only; it does not claim Windows or macOS system signing. A Release contains installers and `SHA256SUMS.txt`, but no `.sig` files or `latest.json`, so the in-app updater never discovers it.
- The repository retains signed-updater code and production release gates for possible future use after certificates and keys are obtained; they are not part of the current distribution promise.
- Do not download “extra signature” files from third parties. A trusted signature cannot be created after the fact without the matching private key, and a fabricated `.sig` will not verify against the public key embedded in the client.

### Windows: SmartScreen or unknown publisher

1. Download only from this repository's GitHub Release and verify `SHA256SUMS.txt` first:

   ```powershell
   Get-FileHash .\Trusted-Carpool_*_x64-setup.exe -Algorithm SHA256
   ```

2. If the browser reports that the file is not commonly downloaded or may be unsafe, confirm that the URL is under `github.com/sunjackson/ai-trusted-carpool`, then choose **Keep**. Depending on the browser version, the action may be under **Show more** or **Keep anyway**.
3. If SmartScreen displays “Windows protected your PC,” select **More info**, confirm the application and filename, then select **Run anyway**.
4. UAC may display “Unknown publisher.” This is expected without an Authenticode certificate; after checking the SHA-256 digest, select **Yes** to continue.
5. Organization-managed devices may hide **Run anyway** through policy. Do not disable Defender/SmartScreen or bypass organization policy; contact the administrator or build from source using the development instructions below.

See [Microsoft Learn](https://learn.microsoft.com/windows/security/operating-system-security/virus-and-threat-protection/microsoft-defender-smartscreen/) for Microsoft's SmartScreen documentation.

### macOS: developer cannot be verified

1. Open the DMG, drag the app into Applications, and try to launch it once from Applications.
2. Close the “developer cannot be verified” or “Apple cannot check it for malicious software” dialog, then open **System Settings → Privacy & Security**.
3. In the Security section, find the blocked app, select **Open Anyway**, authenticate with a password or Touch ID, and select **Open** in the second dialog. The button normally appears only after a blocked launch and remains available for about one hour.
4. If an organization-managed Mac does not offer **Open Anyway**, contact the administrator or build from source. Do not disable Gatekeeper globally or run untrusted `xattr`/`spctl` bypass commands.

Apple documents the supported flow in [Open a Mac app from an unidentified developer](https://support.apple.com/guide/mac-help/open-a-mac-app-from-an-unidentified-developer-mh40616/mac).

### Linux: manual installation

```bash
# AppImage
chmod +x Trusted-Carpool_*_amd64.AppImage
./Trusted-Carpool_*_amd64.AppImage

# Debian / Ubuntu
sudo apt install ./Trusted-Carpool_*_amd64.deb
```

For manual downloads, you can also calculate the digest with `shasum -a 256 <file>` on macOS, `sha256sum <file>` on Linux, or `certutil -hashfile <file> SHA256` on Windows and compare it with the matching entry in the same Release's `SHA256SUMS.txt`. See the [release guide](docs/RELEASE.md) for the future signing design and release gates.

## Local development

```bash
npm ci
npm run dev                 # React/Vite frontend
npm run tauri dev           # desktop app
npm test -- --run           # Vitest
npm run lint
cargo test --manifest-path src-tauri/Cargo.toml --all-targets --all-features
```

## Packaging

```bash
./scripts/build-macos-universal.sh
./scripts/build-windows-cross.sh
./scripts/build-linux-docker.sh
```

GitHub Actions runs frontend, release-manifest, backend, and coordinator tests first, then builds the macOS universal DMG, Windows x64 NSIS, and Linux x64 DEB/AppImage in parallel. Development installers from each run remain in Actions Artifacts for 30 days. Exact `vX.Y.Z` tags currently publish an unsigned stable Release with `SHA256SUMS.txt`; CI creates a draft, uploads and verifies every remote asset, and publishes only after all assets match. After certificates and an updater key are available, the signed/automatic-update gate activates only when `SIGNED_RELEASES_ENABLED` is explicitly enabled.

## Security boundary

A twelve-character seat code has about 60 bits of random entropy, is rate-limited server-side, and only resolves a signed public invite. Every car uses a three-minute coordinator lease renewed every 60 seconds; fixed-window cars also enforce their business end time, while all-day cars remain discoverable only while the host is online. Every accepted claim gets a separate 256-bit session secret bound to the passenger's device identity. Restoring after a host reboot never restores an old session: passengers claim again. Early trusted sharing has no deposits, points, or billing.

The one-click join page is generated only by the configured official origin. The client accepts only `https://p2p.cnaigc.ai/api/v1/carpool/join/...` (and the short `/join/...` path) plus the statically registered `trusted-carpool://join/...` scheme. Links never carry credentials or session secrets; unknown hosts, extra ports, and unsafe characters are rejected before parsing.

Member limits are checked before a request reaches the provider. Subscription quota reads follow the upstream protocol documented by [Sub2API](https://github.com/Wei-Shaw/sub2api) without uploading credentials, account IDs, or raw payloads. Desktop-client integration (backup before write, restore on leave/exit/next launch) is inspired by CC Switch.

Report vulnerabilities through the [private channel](SECURITY.md); please do not open public issues for security problems.

## Self-hosting

Both the coordinator and the TURN relay can be self-hosted. The reference coordinator lives in [`deploy/coordinator/`](deploy/coordinator/) (including the `/api/v1/turn-credentials` ephemeral-credential endpoint), and the client points at your deployment via `TRUSTED_CARPOOL_COORDINATOR_URL`. See [docs/SELF-HOSTING.md](docs/SELF-HOSTING.md) for docker-compose, coturn, CSP changes, and rebuild steps.

## How the project sustains itself

Trusted Carpool follows an Open Core model: **the client, the reference coordinator, and the protocol are Apache-2.0 licensed forever, and self-hosting is never restricted**. The officially hosted `p2p.cnaigc.ai` coordinator/TURN service is free today; optional paid capabilities may later be layered on the hosted service only, never on the open-source code. See [docs/BUSINESS-MODEL.md](docs/BUSINESS-MODEL.md).

## Contributing

Issues and PRs are welcome — see [CONTRIBUTING.md](CONTRIBUTING.md) and [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md). Translating the UI strings (a minimal i18n skeleton already exists) is a great first contribution.

Architecture, product scope, and pricing rules: [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md), [`docs/PRODUCT-BRIEF.md`](docs/PRODUCT-BRIEF.md), [`docs/PRICING-SOURCES.md`](docs/PRICING-SOURCES.md).

## License

[Apache-2.0](LICENSE) · attribution in [NOTICE](NOTICE) · usage notice in [LEGAL.md](LEGAL.md). This project is not affiliated with, endorsed by, or sponsored by Anthropic or OpenAI.
