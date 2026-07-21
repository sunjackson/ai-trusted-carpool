# Trusted Carpool Architecture

## Product Boundary

The desktop app exposes only **host** and **join**. Claude Code and Codex are equal clients. A
host can enable either or both, and each of four seats can run requests concurrently.

## Trust Boundary

- Official account credentials remain on the host device.
- The coordinator stores only signed discovery metadata and expiring mailbox messages.
- A twelve-character code (shown as 4-4-4) has about 60 bits of random entropy and resolves a
  signed public invite; it is not an API credential. The coordinator limits invite resolution to
  60 attempts per minute per client IP, and every code expires with the host's schedule.
- Public coordinator abuse controls (no website login required): invite registration, messaging,
  polling, and TURN credential minting are rate-limited per IP and per peer identity; each owner
  may hold a bounded number of active invites. TURN credentials are issued only after a signed
  proof of possession of the device identity (POST `/api/v1/turn-credentials`), so anonymous
  scrapers cannot mint relay credentials from a guessed `peer_id`. Polling is primarily limited
  per peer so a host and passenger sharing one home NAT do not starve each other's signaling.
- A passenger creates a local P-256/X25519 identity. The host encrypts the seat access grant to
  that identity after the passenger claims the invite.
- Every accepted claim receives a separate 256-bit session secret bound to the passenger identity;
  the short invite code is never used to authenticate proxied API traffic.
- API request bodies use end-to-end encryption between passenger and host. TURN relays packets
  but cannot decrypt application payloads.
- WebRTC signaling payloads (SDP offers/answers, ICE candidates, leave notices) are also
  end-to-end encrypted with per-message X25519 + AES-256-GCM envelopes before they reach the
  coordinator, so the coordinator relays opaque envelopes and never sees session descriptions,
  candidate IP addresses, seat codes, or access ids in transit.
- The passenger's signal poll drops unverifiable or undecryptable messages individually; because
  the coordinator mailbox is drain-on-read, one spam or stale message can never discard the valid
  signals fetched in the same batch. The host bounds its pending-signal queue (256 entries).
- Early trusted sharing has no deposit, points, billing, penalties, or user tiers. The host still
  receives per-seat, per-model input, output, cache-read, cache-write, and request metrics in real
  time.
- Official price figures are USD API list-price estimates calculated per request at the price in
  effect at that time. They are not invoices or Claude/Codex subscription usage. Unknown models
  remain unpriced rather than inheriting a similar model's rate.
- The host can assign independent rolling 5-hour, 24-hour, and 7-day token limits to each seat.
  The relay checks all windows before contacting the provider. Because output usage is known only
  after a response, the final admitted request can overshoot a window; the next request is denied.
- Each completed request appends a local `usage-history.jsonl` event under the app-data directory.
  It stores the car, passenger, tool, model, token categories, and price estimate, but never the
  prompt, response body, credential, session secret, or invite code.
- Official response headers and body chunks are forwarded continuously over the ordered WebRTC
  data channel instead of being buffered. The passenger verifies the final SHA-256 digest before
  accepting a clean stream end; provider-reported usage is applied after the final usage event.

## One-click Flow

1. Host detects local Claude Code/Codex installations and login files without exposing secrets.
   Detection includes GUI-safe npm, Homebrew, NVM, FNM, Volta, pnpm, and Windows npm locations,
   so launching the desktop app outside a shell does not depend on an inherited `PATH`.
2. Host sets a start/end window, creates four signed public invites, and registers them with the
   coordinator.
3. The coordinator exposes a fixed-origin `/api/v1/carpool/join/<code>` launch page. It contains only the short
   code and redirects to the statically registered `trusted-carpool://join/<code>` scheme. The
   desktop app rejects every non-official HTTPS origin, custom-scheme host, port, and malformed code.
4. Passenger opens the official link (or enters one short code), verifies the host signature, and
   sends a signed claim. A locally saved nickname makes repeated friend-to-friend joins one click.
5. Host automatically binds the first valid claimant to that seat and returns an encrypted grant.
6. WebRTC is attempted first; TURN is used automatically when direct connectivity fails.
   The passenger recovers from drops automatically: ICE failure, channel closure, or six missed
   host heartbeats (the 2-second status broadcast) trigger a reconnect loop with exponential
   backoff (six fast attempts, then persistent 30-second retries) that refreshes the time-limited
   TURN credentials before every redial. The host treats a fresh offer from a bound passenger as
   superseding any stale session, so crashed passengers can rejoin immediately. The ride page
   shows the real link state (connected / reconnecting / down) instead of a static label.
7. The passenger's local HTTP proxy streams Claude Code/Codex requests and responses through the
   encrypted data channel without waiting for a complete model response.
8. The host validates the official endpoint, calls the provider locally, extracts provider usage,
   and updates that seat's model-specific counters and official list-price estimate.

## Managed Tool Runtime (zero-setup passengers)

- A passenger machine needs nothing but this app: no Node.js, no manual CLI
  install, no PATH changes, no admin rights, and no personal AI account.
- When the Claude Code or Codex CLI is missing, the app downloads the
  official standalone binary straight from the vendor: Claude Code through
  the `downloads.claude.ai` version manifest (the same flow as the official
  `install.sh`/`install.ps1`), Codex from the GitHub `openai/codex` release
  assets. The app never bundles or redistributes these binaries.
- Downloads are streamed with live progress and can be cancelled. Every file
  is SHA-256-verified against the official manifest checksum or GitHub asset
  digest before activation; a mismatch deletes the file. Installs land in
  `<app-data>/tools/<tool>/<version>/` with an atomically switched `current`
  pointer and a `provenance.json` recording source URL, checksum, version,
  and time.
- A user-managed system install always takes precedence over the app-managed
  copy. Managed Claude Code launches with its own auto-updater disabled;
  updates flow through the app instead (release metadata cached for 24 hours
  and refreshed in the background, one-click update).
- Update safety: installs always prefer freshly fetched official metadata
  (the cache is only an offline fallback); a new binary must pass a
  `--version` smoke test before the `current` pointer flips, so a broken
  release can never replace a working one; the most recent previous version
  is retained as a rollback path and older versions are pruned.
- Launch integrity: before every launch, an app-managed binary is re-hashed
  and compared against the checksum recorded in `provenance.json`. Any
  mismatch clears the `current` pointer, refuses the launch, and returns the
  UI to a fresh one-click install.
- The managed runtime directory is restricted to the current user (0700 on
  Unix), matching the existing 0600/0700 treatment of the device identity,
  usage history, and client-config backups.
- App self-update awareness: the app compares its own version against the
  latest GitHub release (cached 24 hours) and shows a title-bar notice; the
  "open releases" action uses a pinned constant URL, never API-provided data.
- npm installation (`@anthropic-ai/claude-code`, `@openai/codex`) remains
  only as a fallback when the official channel is unreachable and Node.js
  already exists.

## Quota Visibility

- Claude OAuth quota is read from `https://api.anthropic.com/api/oauth/usage`; Codex/ChatGPT OAuth
  quota is read from `https://chatgpt.com/backend-api/wham/usage`. This follows the upstream shape
  documented by the [Sub2API implementation](https://github.com/Wei-Shaw/sub2api).
- The host queries those fixed official HTTPS endpoints with local credentials. Credentials,
  ChatGPT account IDs, and raw provider payloads never leave the host.
- Every two seconds the host sends each connected member a sanitized WebRTC snapshot containing
  the common account quota plus only that member's own usage and seat limits. Members cannot query
  another seat's details.
- API-key authentication has no Claude/Codex subscription quota endpoint, so the UI reports
  “unsupported” rather than inventing a remaining percentage. A transient refresh error preserves
  the last successful snapshot and labels it stale.

## Cross-platform Delivery

Tauri 2 and Rust provide one codebase for macOS, Windows, and Linux. Platform-specific code is
limited to executable discovery, detached process launch, secure file permissions, and packaging.
The deep-link and single-instance plugins route cold and warm launch URLs to the same validated
join handler. A native tray/menu-bar item mirrors idle, hosting occupancy, riding, or combined
state once per second. Closing the main window hides it instead of stopping an active car or ride;
the tray menu remains the explicit reopen/quit surface.
