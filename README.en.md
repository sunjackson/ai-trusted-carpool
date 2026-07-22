# Trusted Carpool

[中文](README.md) | **English**

[![CI](https://github.com/sunjackson/ai-trusted-carpool/actions/workflows/build-desktop.yml/badge.svg)](https://github.com/sunjackson/ai-trusted-carpool/actions/workflows/build-desktop.yml)
[![CodeQL](https://github.com/sunjackson/ai-trusted-carpool/actions/workflows/codeql.yml/badge.svg)](https://github.com/sunjackson/ai-trusted-carpool/actions/workflows/codeql.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Release](https://img.shields.io/github/v/release/sunjackson/ai-trusted-carpool?include_prereleases)](https://github.com/sunjackson/ai-trusted-carpool/releases)

> [!WARNING]
> **Before you use this**: sharing one official Claude/Codex subscription account among several people directly conflicts with Anthropic's Consumer Terms and OpenAI's Terms of Use, which prohibit sharing account credentials or making your account available to anyone else. Your account may be rate-limited or permanently banned without a refund. Use it only with people you personally trust; the account owner bears all risk. See [LEGAL.md](LEGAL.md).

A desktop app for sharing a locally signed-in Claude Code / Codex account among people who already know and trust each other. The host picks an explicit open window, starts a "car" with one click, and gets four seat codes; a passenger enters a seat code and can open either tool — both can run at the same time. **The code and self-hosting are free and open source forever** — see the [business model](docs/BUSINESS-MODEL.md).

![UI design board](design/ui-design-board-v4.png)

## Table of contents

- [Highlights](#highlights)
- [Install](#install)
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
- WebRTC direct connection first, automatic TURN fallback; both application payloads and connection signaling are end-to-end encrypted (the coordinator never sees SDP or candidate IPs).
- Automatic drop recovery: passenger-side heartbeat detection plus exponential-backoff reconnects (with fresh TURN credentials), and the ride page shows the real link state.
- Credentials never leave the host machine; only official Anthropic and OpenAI/ChatGPT endpoints are allowed.
- Real-time per-member → per-tool → per-model stats: requests, input, output, cache read/write, and official USD list-price estimates.
- The member list stays concise (totals, requests, price, key limits); click a member for the per-model breakdown.
- The host can set independent rolling 5-hour / 24-hour / 7-day token limits per member.
- Host and online members see the same official Claude/Codex subscription quota; API-key auth shows "unsupported" instead of invented numbers.
- The local append-only history stores usage metadata only — never prompts, response bodies, credentials, session secrets, or seat codes.
- The macOS menu bar, Windows tray, and Linux status area mirror idle/hosting/riding state; closing the main window keeps the app resident.

## Install

Download the installer for your platform from [GitHub Releases](https://github.com/sunjackson/ai-trusted-carpool/releases) (macOS universal DMG, Windows x64 NSIS, Linux x64 DEB/AppImage) and verify `SHA256SUMS.txt`. Installers from every CI run are also kept as Actions artifacts for 30 days.

> macOS builds are not yet Apple-notarized and remain manual installs/updates. Tagged Windows releases are signed with a pinned certificate fingerprint and verified with `signtool`; ordinary branch artifacts are unsigned development builds. See [docs/RELEASE.md](docs/RELEASE.md) for status and required Secrets.

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

GitHub Actions runs frontend, release-manifest, backend, and coordinator tests first, then builds the macOS universal DMG, Windows x64 NSIS, and Linux x64 DEB/AppImage in parallel. A matching `vX.Y.Z` tag enters the signing gate, producing Windows/Linux updater signatures, a `latest.json` containing only NSIS and AppImage targets, `SHA256SUMS.txt`, and bilingual release notes. macOS and DEB updates remain manual.

## Security boundary

A twelve-character seat code has about 60 bits of random entropy, is rate-limited server-side, and expires with the host's schedule; it only resolves a signed public invite. Every accepted claim gets a separate 256-bit session secret bound to the passenger's device identity. Early trusted sharing has no deposits, points, or billing.

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
