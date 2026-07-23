'use strict';

const crypto = require('crypto');
const http = require('http');
const { URL } = require('url');

const DEFAULT_MAX_TTL_MS = 120_000;
const DEFAULT_INVITE_TTL_MS = 24 * 60 * 60 * 1000;
const MAX_BODY_BYTES = 96 * 1024;
const MAX_INVITES = 20_000;
const MAX_INVITES_PER_OWNER = 16;
const MAX_MESSAGES_PER_PEER = 128;
const DEFAULT_RESOLVE_RATE_LIMIT = 60;
const DEFAULT_REGISTER_RATE_LIMIT = 30;
// WebRTC trickle ICE can burst many small messages; keep IP soft and peer hard.
const DEFAULT_MESSAGE_RATE_LIMIT = 480;
const DEFAULT_MESSAGE_PEER_RATE_LIMIT = 180;
// Poll is primarily per peer (host + passenger share one public IP on home Wi-Fi).
const DEFAULT_POLL_RATE_LIMIT = 600;
const DEFAULT_POLL_PEER_RATE_LIMIT = 180;
const DEFAULT_TURN_RATE_LIMIT = 30;
const RATE_WINDOW_MS = 60_000;
const DEFAULT_TURN_TTL_SECONDS = 3600;
const MAX_TURN_TTL_SECONDS = 24 * 60 * 60;
const DEFAULT_DESKTOP_RELEASE_VERSION = '0.0.7';
const DESKTOP_RELEASE_REPOSITORY = 'sunjackson/ai-trusted-carpool';
const ALLOWED_MESSAGE_KINDS = new Set([
  'carpool_claim',
  'carpool_access',
  'webrtc_offer',
  'webrtc_answer',
  'ice_candidate',
  'hangup',
]);

function nowMs() { return Date.now(); }

function validPeerId(value) {
  return typeof value === 'string' && /^p2p-[A-Za-z0-9_-]{12,64}$/.test(value);
}

function validCode(value) {
  return typeof value === 'string' && /^[A-HJ-NP-Z2-9]{12}$/.test(value);
}

function json(res, status, payload) {
  const body = JSON.stringify(payload);
  res.writeHead(status, {
    'content-type': 'application/json; charset=utf-8',
    'cache-control': 'no-store',
    'x-content-type-options': 'nosniff',
  });
  res.end(body);
}

function error(res, status, message, headers = {}) {
  const body = JSON.stringify({ error: message });
  res.writeHead(status, {
    'content-type': 'application/json; charset=utf-8',
    'cache-control': 'no-store',
    'x-content-type-options': 'nosniff',
    ...headers,
  });
  res.end(body);
}

function html(res, status, body, options = {}) {
  const directives = [
    "default-src 'none'",
    "style-src 'unsafe-inline'",
    "base-uri 'none'",
    "form-action 'none'",
    "frame-ancestors 'none'",
  ];
  if (options.scriptNonce) directives.push(`script-src 'nonce-${options.scriptNonce}'`);
  res.writeHead(status, {
    'content-type': 'text/html; charset=utf-8',
    'cache-control': 'no-store',
    'content-security-policy': directives.join('; '),
    'referrer-policy': 'no-referrer',
    'x-content-type-options': 'nosniff',
    'x-frame-options': 'DENY',
  });
  res.end(body);
}

function desktopReleaseVersion(value) {
  const version = String(value || '').trim();
  return /^\d+\.\d+\.\d+$/.test(version) ? version : DEFAULT_DESKTOP_RELEASE_VERSION;
}

function desktopDownloadUrls(version) {
  const releaseVersion = desktopReleaseVersion(version);
  const base = `https://github.com/${DESKTOP_RELEASE_REPOSITORY}/releases/download/v${releaseVersion}`;
  return {
    windows: `${base}/Trusted-Carpool_${releaseVersion}_x64-setup.exe`,
    macos: `${base}/Trusted-Carpool_${releaseVersion}_universal.dmg`,
    appImage: `${base}/Trusted-Carpool_${releaseVersion}_amd64.AppImage`,
    deb: `${base}/Trusted-Carpool_${releaseVersion}_amd64.deb`,
    checksums: `${base}/SHA256SUMS.txt`,
    release: `https://github.com/${DESKTOP_RELEASE_REPOSITORY}/releases/tag/v${releaseVersion}`,
  };
}

function joinPage(code, options = {}) {
  if (!validCode(code)) throw new Error('invalid join code');
  const scriptNonce = options.scriptNonce || crypto.randomBytes(18).toString('base64');
  if (!/^[A-Za-z0-9+/=_-]{16,64}$/.test(scriptNonce)) throw new Error('invalid script nonce');
  const releaseVersion = desktopReleaseVersion(options.releaseVersion);
  const downloads = desktopDownloadUrls(releaseVersion);
  const deepLink = `trusted-carpool://join/${code}`;
  return `<!doctype html>
<html lang="zh-CN">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width,initial-scale=1">
  <title>正在上车 · 可信拼车</title>
  <style>
    :root{color-scheme:dark;font-family:-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;background:#111315;color:#f7f7f4}
    *{box-sizing:border-box}body{min-height:100vh;margin:0;display:flex;justify-content:center;padding:24px;background:radial-gradient(circle at 50% 12%,#2f291d 0,#171819 36%,#101112 72%)}
    main{width:min(620px,100%);margin:auto 0;padding:40px 32px;border:1px solid #35383b;border-radius:24px;background:#1b1d1f;box-shadow:0 24px 80px #0008;text-align:center}
    .mark{width:62px;height:62px;margin:0 auto 20px;display:grid;place-items:center;border-radius:20px;background:#d8ad58;color:#15120d;font-size:30px;font-weight:800}
    h1{margin:0 0 10px;font-size:27px}h2{margin:28px 0 8px;font-size:20px}p{margin:0 0 22px;color:#aeb1b4;line-height:1.7}.code{display:block;margin:0 0 22px;color:#e7c67e;font:700 15px ui-monospace,SFMono-Regular,Menlo,monospace;letter-spacing:2px}
    .open-client{display:block;padding:14px 18px;border-radius:12px;background:#d8ad58;color:#17130c;text-decoration:none;font-weight:800}.status{min-height:24px;margin:14px 0 0;color:#8e9499;font-size:13px}
    .downloads[hidden]{display:none}.download-copy{margin-bottom:16px}.download-list{display:grid;grid-template-columns:repeat(2,minmax(0,1fr));gap:10px;text-align:left}
    .download-card{position:relative;display:flex;min-height:72px;flex-direction:column;justify-content:center;padding:13px 14px;border:1px solid #34383c;border-radius:12px;background:#24272a;color:#f5f1e7;text-decoration:none}.download-card:hover{border-color:#8b7442;background:#2b2a25}.download-card.recommended{border-color:#d8ad58;box-shadow:0 0 0 1px #d8ad5833}.download-card strong{font-size:14px}.download-card span{margin-top:4px;color:#92989d;font-size:11px}.badge{display:none;position:absolute;right:10px;top:9px;color:#e7c67e;font-size:10px;font-weight:800}.recommended .badge{display:block}
    .security-note{display:block;margin-top:16px;color:#777d82;font-size:11px;line-height:1.6}.security-note a{color:#bda66d}.manual-release{margin-top:8px!important;color:#6f757a!important;font-size:11px}noscript{display:block;margin-top:18px;color:#d8ad58}
    @media(max-width:560px){main{padding:30px 20px}.download-list{grid-template-columns:1fr}}
  </style>
</head>
<body>
  <main>
    <div class="mark">车</div>
    <h1>正在唤起可信拼车</h1>
    <p>客户端会自动确认这辆车，无需再输入上车码。</p>
    <span class="code">${code}</span>
    <a id="open-client" class="open-client" href="${deepLink}">打开可信拼车并上车</a>
    <p id="client-status" class="status" role="status" aria-live="polite">正在检测本机客户端并尝试打开…</p>
    <section id="downloads" class="downloads" hidden>
      <h2 id="download-title">没有看到客户端打开？</h2>
      <p id="download-copy" class="download-copy">已按当前系统标出推荐安装包。安装后重新打开本页面即可继续上车。</p>
      <div class="download-list">
        <a class="download-card" data-platform="windows" href="${downloads.windows}" target="_blank" rel="noopener noreferrer"><span class="badge">推荐</span><strong>Windows x64</strong><span>NSIS 安装程序 · .exe</span></a>
        <a class="download-card" data-platform="macos" href="${downloads.macos}" target="_blank" rel="noopener noreferrer"><span class="badge">推荐</span><strong>macOS 通用版</strong><span>Intel + Apple 芯片 · .dmg</span></a>
        <a class="download-card" data-platform="linux" href="${downloads.appImage}" target="_blank" rel="noopener noreferrer"><span class="badge">推荐</span><strong>Linux x64 AppImage</strong><span>免安装便携版</span></a>
        <a class="download-card" data-platform="linux" href="${downloads.deb}" target="_blank" rel="noopener noreferrer"><span class="badge">推荐</span><strong>Debian / Ubuntu x64</strong><span>DEB 软件包</span></a>
      </div>
      <small class="security-note">当前为未签名手动版 v${releaseVersion}。请只从本项目 GitHub 下载，并用 <a href="${downloads.checksums}" target="_blank" rel="noopener noreferrer">SHA256SUMS.txt</a> 核对文件；不要关闭系统安全防护。</small>
      <p class="manual-release"><a href="${downloads.release}" target="_blank" rel="noopener noreferrer">查看全部平台与安装说明</a></p>
    </section>
    <noscript>浏览器未启用 JavaScript。请先点击“打开可信拼车”，未安装时前往 <a href="${downloads.release}">GitHub Release</a> 下载客户端。</noscript>
    <small class="security-note">只加入你认识并信任的人发起的车队。</small>
  </main>
  <script nonce="${scriptNonce}">
    (() => {
      const deepLink = ${JSON.stringify(deepLink)};
      const status = document.getElementById('client-status');
      const downloads = document.getElementById('downloads');
      const title = document.getElementById('download-title');
      const copy = document.getElementById('download-copy');
      const openClient = document.getElementById('open-client');
      let fallbackTimer = null;

      function detectPlatform() {
        const userAgentDataPlatform = navigator.userAgentData && navigator.userAgentData.platform
          ? navigator.userAgentData.platform
          : '';
        const platformHint = [userAgentDataPlatform, navigator.platform || '', navigator.userAgent || '']
          .join(' ')
          .toLowerCase();
        if (
          /android|iphone|ipad|ipod/.test(platformHint)
          || ((navigator.maxTouchPoints || 0) > 1 && /macintosh|macintel/.test(platformHint))
        ) return 'mobile';
        if (/windows|win32|win64/.test(platformHint)) return 'windows';
        if (/macintosh|macintel|mac os x|macos/.test(platformHint)) return 'macos';
        if (/linux|x11/.test(platformHint)) {
          return /aarch64|arm64|armv8/.test(platformHint) ? 'linux-arm' : 'linux';
        }
        return 'other';
      }

      const platform = detectPlatform();

      function revealDownloads() {
        if (document.hidden) return;
        downloads.hidden = false;
        document.querySelectorAll('[data-platform]').forEach(card => {
          card.classList.toggle('recommended', card.dataset.platform === platform);
        });
        if (platform === 'windows') {
          title.textContent = '未检测到客户端，下载 Windows 版';
          copy.textContent = '推荐 Windows x64 安装程序。安装完成后重新打开上车链接。';
        } else if (platform === 'macos') {
          title.textContent = '未检测到客户端，下载 macOS 版';
          copy.textContent = '通用 DMG 同时支持 Apple 芯片和 Intel Mac。';
        } else if (platform === 'linux') {
          title.textContent = '未检测到客户端，下载 Linux 版';
          copy.textContent = '优先选择 AppImage；Debian 或 Ubuntu 也可使用 DEB。';
        } else if (platform === 'linux-arm') {
          title.textContent = '暂未提供 Linux ARM 安装包';
          copy.textContent = '当前公开安装包仅支持 Linux x64；请勿下载架构不匹配的文件。';
        } else if (platform === 'mobile') {
          title.textContent = '请在电脑上打开上车链接';
          copy.textContent = '可信拼车当前支持 Windows、macOS 和 Linux 桌面系统。';
        } else {
          title.textContent = '选择你的电脑系统';
          copy.textContent = '未能识别当前系统，请手动选择对应安装包。';
        }
        status.textContent = platform === 'mobile'
          ? '当前手机系统没有对应客户端。'
          : '没有看到客户端打开时，请下载安装后重试。';
      }

      function scheduleFallback() {
        if (fallbackTimer !== null) window.clearTimeout(fallbackTimer);
        fallbackTimer = window.setTimeout(revealDownloads, 1800);
      }

      function markClientOpened() {
        if (fallbackTimer !== null) window.clearTimeout(fallbackTimer);
        downloads.hidden = true;
        status.textContent = '已向本机客户端发送上车请求。';
      }

      document.addEventListener('visibilitychange', () => {
        if (document.hidden) markClientOpened();
      });
      window.addEventListener('pagehide', markClientOpened);
      openClient.addEventListener('click', scheduleFallback);

      if (platform === 'mobile') {
        revealDownloads();
      } else {
        window.setTimeout(() => {
          scheduleFallback();
          window.location.href = deepLink;
        }, 120);
      }
    })();
  </script>
</body>
</html>`;
}

function readJson(req) {
  return new Promise((resolve, reject) => {
    let size = 0;
    const chunks = [];
    req.on('data', chunk => {
      size += chunk.length;
      if (size > MAX_BODY_BYTES) {
        const cause = new Error('body too large');
        cause.statusCode = 413;
        reject(cause);
        req.destroy();
        return;
      }
      chunks.push(chunk);
    });
    req.on('end', () => {
      try { resolve(JSON.parse(Buffer.concat(chunks).toString('utf8'))); }
      catch (cause) { reject(cause); }
    });
    req.on('error', reject);
  });
}

function peerIdFromPublicKey(publicKeyBase64) {
  const bytes = Buffer.from(publicKeyBase64, 'base64');
  const hash = crypto.createHash('sha256').update(bytes).digest().subarray(0, 16);
  return `p2p-${hash.toString('base64url')}`;
}

// coturn "TURN REST API" ephemeral credentials: username is
// "<expiry-unixtime>:<peer_id>" and the credential is
// base64(hmac-sha1(static-auth-secret, username)).
function turnRestCredentials(secret, peerId, ttlSeconds, nowMsValue) {
  const expiresAt = Math.floor(nowMsValue / 1000) + ttlSeconds;
  const username = `${expiresAt}:${peerId}`;
  const credential = crypto.createHmac('sha1', secret).update(username).digest('base64');
  return { username, credential, expires_at_s: expiresAt };
}

function parseTurnUrls(value) {
  return String(value || '')
    .split(',')
    .map(entry => entry.trim())
    .filter(entry => /^turns?:/.test(entry));
}

function p256PublicKey(rawBase64) {
  const raw = Buffer.from(rawBase64, 'base64');
  if (raw.length !== 65 || raw[0] !== 4) throw new Error('expected uncompressed P-256 key');
  const prefix = Buffer.from('3059301306072a8648ce3d020106082a8648ce3d030107034200', 'hex');
  return crypto.createPublicKey({ key: Buffer.concat([prefix, raw]), format: 'der', type: 'spki' });
}

function verifyPeerSignature(peerId, publicKeyBase64, payload, signatureBase64) {
  if (!validPeerId(peerId)) return 'invalid peer_id';
  if (typeof publicKeyBase64 !== 'string' || publicKeyBase64.length < 40) return 'invalid public_key';
  if (peerIdFromPublicKey(publicKeyBase64) !== peerId) return 'peer_id does not match public_key';
  try {
    const verifier = crypto.createVerify('SHA256');
    verifier.update(Buffer.from(payload));
    verifier.end();
    return verifier.verify(p256PublicKey(publicKeyBase64), Buffer.from(signatureBase64 || '', 'base64'))
      ? null
      : 'invalid signature';
  } catch (cause) {
    return `signature decode failed: ${cause.message}`;
  }
}

function canonicalInvite(input) {
  return JSON.stringify({
    code: input.code,
    owner_peer_id: input.owner_peer_id,
    owner_public_key: input.owner_public_key,
    owner_encryption_public_key: input.owner_encryption_public_key,
    car_id: input.car_id,
    seat_no: input.seat_no,
    payload_base64: input.payload_base64,
    expires_at_ms: input.expires_at_ms,
    timestamp_ms: input.timestamp_ms,
  });
}

function canonicalMessage(input) {
  return JSON.stringify({
    from_peer_id: input.from_peer_id,
    to_peer_id: input.to_peer_id,
    public_key: input.public_key,
    kind: input.kind,
    payload_json: input.payload_json,
    ttl_ms: input.ttl_ms,
    timestamp_ms: input.timestamp_ms,
  });
}

function canonicalPoll(input) {
  return JSON.stringify({
    peer_id: input.peer_id,
    public_key: input.public_key,
    after_ms: input.after_ms ?? null,
    limit: input.limit ?? null,
    timestamp_ms: input.timestamp_ms,
  });
}

function canonicalTurnCredentials(input) {
  return JSON.stringify({
    peer_id: input.peer_id,
    public_key: input.public_key,
    timestamp_ms: input.timestamp_ms,
  });
}

function validateInvite(input, clock) {
  if (!validCode(input.code)) return 'invalid code';
  if (!validPeerId(input.owner_peer_id)) return 'invalid owner_peer_id';
  if (typeof input.owner_encryption_public_key !== 'string' || input.owner_encryption_public_key.length < 40) return 'invalid owner_encryption_public_key';
  if (typeof input.car_id !== 'string' || input.car_id.length < 16 || input.car_id.length > 80) return 'invalid car_id';
  if (!Number.isInteger(input.seat_no) || input.seat_no < 1 || input.seat_no > 4) return 'invalid seat_no';
  if (typeof input.payload_base64 !== 'string' || input.payload_base64.length < 20 || input.payload_base64.length > 24_000) return 'invalid payload_base64';
  if (!Number.isSafeInteger(input.expires_at_ms) || input.expires_at_ms <= clock || input.expires_at_ms > clock + DEFAULT_INVITE_TTL_MS) return 'invalid expires_at_ms';
  if (!Number.isSafeInteger(input.timestamp_ms) || Math.abs(clock - input.timestamp_ms) > 300_000) return 'stale timestamp_ms';
  return verifyPeerSignature(input.owner_peer_id, input.owner_public_key, canonicalInvite(input), input.signature);
}

function validateMessage(input, clock) {
  if (!validPeerId(input.from_peer_id) || !validPeerId(input.to_peer_id)) return 'invalid peer id';
  if (input.from_peer_id === input.to_peer_id) return 'sender and recipient must differ';
  if (!ALLOWED_MESSAGE_KINDS.has(input.kind)) return 'invalid message kind';
  if (typeof input.payload_json !== 'string' || Buffer.byteLength(input.payload_json) > 64 * 1024) return 'invalid payload_json';
  try { JSON.parse(input.payload_json); } catch { return 'payload_json must be json'; }
  if (!Number.isSafeInteger(input.ttl_ms) || input.ttl_ms < 1000 || input.ttl_ms > DEFAULT_MAX_TTL_MS) return 'invalid ttl_ms';
  if (!Number.isSafeInteger(input.timestamp_ms) || Math.abs(clock - input.timestamp_ms) > 300_000) return 'stale timestamp_ms';
  return verifyPeerSignature(input.from_peer_id, input.public_key, canonicalMessage(input), input.signature);
}

function validatePoll(input, clock) {
  if (!validPeerId(input.peer_id)) return 'invalid peer_id';
  if (input.after_ms != null && (!Number.isSafeInteger(input.after_ms) || input.after_ms < 0)) return 'invalid after_ms';
  if (input.limit != null && (!Number.isSafeInteger(input.limit) || input.limit < 1 || input.limit > MAX_MESSAGES_PER_PEER)) return 'invalid limit';
  if (!Number.isSafeInteger(input.timestamp_ms) || Math.abs(clock - input.timestamp_ms) > 300_000) return 'stale timestamp_ms';
  return verifyPeerSignature(input.peer_id, input.public_key, canonicalPoll(input), input.signature);
}

function validateTurnCredentials(input, clock) {
  if (!validPeerId(input.peer_id)) return 'invalid peer_id';
  if (!Number.isSafeInteger(input.timestamp_ms) || Math.abs(clock - input.timestamp_ms) > 300_000) return 'stale timestamp_ms';
  return verifyPeerSignature(input.peer_id, input.public_key, canonicalTurnCredentials(input), input.signature);
}

function createCoordinator(options = {}) {
  const invites = new Map();
  const mailboxes = new Map();
  const seenMessages = new Map();
  const rateBuckets = new Map();
  const clock = options.clock || nowMs;
  const trustProxy = options.trustProxy ?? process.env.TRUSTED_CARPOOL_TRUST_PROXY === '1';
  const resolveRateLimit = options.resolveRateLimit ?? DEFAULT_RESOLVE_RATE_LIMIT;
  const registerRateLimit = options.registerRateLimit ?? DEFAULT_REGISTER_RATE_LIMIT;
  const messageRateLimit = options.messageRateLimit ?? DEFAULT_MESSAGE_RATE_LIMIT;
  const messagePeerRateLimit = options.messagePeerRateLimit ?? DEFAULT_MESSAGE_PEER_RATE_LIMIT;
  const pollRateLimit = options.pollRateLimit ?? DEFAULT_POLL_RATE_LIMIT;
  const pollPeerRateLimit = options.pollPeerRateLimit ?? DEFAULT_POLL_PEER_RATE_LIMIT;
  const turnRateLimit = options.turnRateLimit ?? DEFAULT_TURN_RATE_LIMIT;
  const maxInvitesPerOwner = options.maxInvitesPerOwner ?? MAX_INVITES_PER_OWNER;
  const configuredDesktopReleaseVersion = desktopReleaseVersion(
    options.desktopReleaseVersion ?? process.env.TRUSTED_CARPOOL_DESKTOP_RELEASE_VERSION
  );
  const turnSecret = options.turnSecret ?? process.env.TRUSTED_CARPOOL_TURN_SECRET ?? '';
  const turnUrls = options.turnUrls ?? parseTurnUrls(process.env.TRUSTED_CARPOOL_TURN_URLS);
  const configuredTurnTtl = Number(
    options.turnTtlSeconds ?? process.env.TRUSTED_CARPOOL_TURN_TTL_SECONDS ?? DEFAULT_TURN_TTL_SECONDS
  );
  const turnTtlSeconds =
    Number.isSafeInteger(configuredTurnTtl) && configuredTurnTtl > 0
      ? Math.min(configuredTurnTtl, MAX_TURN_TTL_SECONDS)
      : DEFAULT_TURN_TTL_SECONDS;

  function clientIp(req) {
    if (trustProxy) {
      const forwarded = req.headers['x-forwarded-for'];
      if (typeof forwarded === 'string' && forwarded.trim()) return forwarded.split(',')[0].trim();
    }
    return req.socket.remoteAddress || 'unknown';
  }

  function allowRate(key, limit) {
    const now = clock();
    const existing = rateBuckets.get(key);
    if (!existing || existing.resetAt <= now) {
      rateBuckets.set(key, { count: 1, resetAt: now + RATE_WINDOW_MS });
      return true;
    }
    existing.count += 1;
    return existing.count <= limit;
  }

  function activeInviteCountForOwner(ownerPeerId) {
    let count = 0;
    for (const invite of invites.values()) {
      if (invite.owner_peer_id === ownerPeerId) count += 1;
    }
    return count;
  }

  function issueTurnCredentials(peerId, res) {
    const credentials = turnRestCredentials(turnSecret, peerId, turnTtlSeconds, clock());
    return json(res, 200, {
      urls: turnUrls,
      username: credentials.username,
      credential: credentials.credential,
      ttl_seconds: turnTtlSeconds,
    });
  }

  function cleanup() {
    const now = clock();
    for (const [code, invite] of invites) if (invite.expires_at_ms <= now) invites.delete(code);
    for (const [fingerprint, expiresAt] of seenMessages) if (expiresAt <= now) seenMessages.delete(fingerprint);
    for (const [key, bucket] of rateBuckets) if (bucket.resetAt <= now) rateBuckets.delete(key);
    for (const [peerId, messages] of mailboxes) {
      const active = messages.filter(message => message.expires_at_ms > now);
      if (active.length) mailboxes.set(peerId, active); else mailboxes.delete(peerId);
    }
  }

  async function handle(req, res) {
    cleanup();
    const url = new URL(req.url, `http://${req.headers.host || 'localhost'}`);
    if (req.method === 'GET' && ['/health', '/api/v1/health'].includes(url.pathname)) {
      return json(res, 200, { ok: true, invites: invites.size, messages: [...mailboxes.values()].reduce((sum, list) => sum + list.length, 0), now_ms: clock() });
    }

    // Unsigned GET is rejected: callers must prove possession of the peer
    // identity with a signed POST so anonymous scrapers cannot mint TURN.
    if (req.method === 'GET' && url.pathname === '/api/v1/turn-credentials') {
      return error(res, 405, 'turn credentials require a signed POST', { allow: 'POST' });
    }

    if (req.method === 'POST' && url.pathname === '/api/v1/turn-credentials') {
      if (!turnSecret || turnUrls.length === 0) {
        return error(res, 404, 'turn relay is not configured');
      }
      const ip = clientIp(req);
      if (!allowRate(`turn:${ip}`, turnRateLimit)) {
        return error(res, 429, 'too many turn credential requests', { 'retry-after': '60' });
      }
      let input;
      try { input = await readJson(req); } catch (cause) { return error(res, cause.statusCode || 400, cause.message); }
      const validation = validateTurnCredentials(input, clock());
      if (validation) return error(res, 400, validation);
      if (!allowRate(`turn-peer:${input.peer_id}`, turnRateLimit)) {
        return error(res, 429, 'too many turn credential requests for peer', { 'retry-after': '60' });
      }
      return issueTurnCredentials(input.peer_id, res);
    }

    const joinMatch = url.pathname.match(/^(?:\/join|\/api\/v1\/carpool\/join)\/([A-HJ-NP-Z2-9]{12})$/);
    if (req.method === 'GET' && joinMatch) {
      if (!allowRate(`join:${clientIp(req)}`, resolveRateLimit)) {
        return error(res, 429, 'too many join link lookups', { 'retry-after': '60' });
      }
      if (!invites.has(joinMatch[1])) return error(res, 404, 'invite not found or expired');
      const scriptNonce = crypto.randomBytes(18).toString('base64');
      return html(
        res,
        200,
        joinPage(joinMatch[1], {
          releaseVersion: configuredDesktopReleaseVersion,
          scriptNonce,
        }),
        { scriptNonce }
      );
    }

    if (req.method === 'POST' && url.pathname === '/api/v1/carpool/invites') {
      const ip = clientIp(req);
      if (!allowRate(`register:${ip}`, registerRateLimit)) {
        return error(res, 429, 'too many invite registrations', { 'retry-after': '60' });
      }
      let input;
      try { input = await readJson(req); } catch (cause) { return error(res, cause.statusCode || 400, cause.message); }
      const validation = validateInvite(input, clock());
      if (validation) return error(res, 400, validation);
      if (!allowRate(`register-peer:${input.owner_peer_id}`, registerRateLimit)) {
        return error(res, 429, 'too many invite registrations for peer', { 'retry-after': '60' });
      }
      const existing = invites.get(input.code);
      if (existing && existing.owner_peer_id !== input.owner_peer_id) return error(res, 409, 'code collision');
      if (!existing && invites.size >= MAX_INVITES) return error(res, 503, 'invite capacity reached');
      if (!existing && activeInviteCountForOwner(input.owner_peer_id) >= maxInvitesPerOwner) {
        return error(res, 429, 'owner invite quota reached');
      }
      const record = { ...input, signature: input.signature, registered_at_ms: clock() };
      invites.set(input.code, record);
      return json(res, 200, { registered: true, invite: record });
    }

    const inviteMatch = url.pathname.match(/^\/api\/v1\/carpool\/invites\/([A-HJ-NP-Z2-9]{12})$/);
    if (req.method === 'GET' && inviteMatch) {
      if (!allowRate(`resolve:${clientIp(req)}`, resolveRateLimit)) {
        return error(res, 429, 'too many invite lookups', { 'retry-after': '60' });
      }
      const invite = invites.get(inviteMatch[1]);
      if (!invite) return error(res, 404, 'invite not found or expired');
      return json(res, 200, { invite });
    }

    if (req.method === 'POST' && url.pathname === '/api/v1/carpool/messages') {
      const ip = clientIp(req);
      if (!allowRate(`message:${ip}`, messageRateLimit)) {
        return error(res, 429, 'too many messages', { 'retry-after': '60' });
      }
      let input;
      try { input = await readJson(req); } catch (cause) { return error(res, cause.statusCode || 400, cause.message); }
      const validation = validateMessage(input, clock());
      if (validation) return error(res, 400, validation);
      if (!allowRate(`message-peer:${input.from_peer_id}`, messagePeerRateLimit)) {
        return error(res, 429, 'too many messages from peer', { 'retry-after': '60' });
      }
      const replayFingerprint = crypto.createHash('sha256')
        .update(`${input.from_peer_id}\n${input.signature}`)
        .digest('base64url');
      if (seenMessages.has(replayFingerprint)) return error(res, 409, 'duplicate signed message');
      seenMessages.set(replayFingerprint, clock() + input.ttl_ms);
      const queue = mailboxes.get(input.to_peer_id) || [];
      const createdAt = clock();
      const message = {
        id: crypto.randomUUID(),
        from_peer_id: input.from_peer_id,
        to_peer_id: input.to_peer_id,
        public_key: input.public_key,
        kind: input.kind,
        payload_json: input.payload_json,
        ttl_ms: input.ttl_ms,
        signature: input.signature,
        timestamp_ms: input.timestamp_ms,
        created_at_ms: createdAt,
        expires_at_ms: createdAt + input.ttl_ms,
      };
      queue.push(message);
      while (queue.length > MAX_MESSAGES_PER_PEER) queue.shift();
      mailboxes.set(input.to_peer_id, queue);
      return json(res, 200, { queued: true, message });
    }

    if (req.method === 'POST' && url.pathname === '/api/v1/carpool/messages/poll') {
      let input;
      try { input = await readJson(req); } catch (cause) { return error(res, cause.statusCode || 400, cause.message); }
      const validation = validatePoll(input, clock());
      if (validation) return error(res, 400, validation);
      // Peer first: one shared NAT must not starve the passenger while the host polls.
      if (!allowRate(`poll-peer:${input.peer_id}`, pollPeerRateLimit)) {
        return error(res, 429, 'too many polls for peer', { 'retry-after': '60' });
      }
      if (!allowRate(`poll:${clientIp(req)}`, pollRateLimit)) {
        return error(res, 429, 'too many polls', { 'retry-after': '60' });
      }
      const queue = mailboxes.get(input.peer_id) || [];
      const after = input.after_ms || 0;
      const limit = Math.min(input.limit || 64, MAX_MESSAGES_PER_PEER);
      const selected = queue.filter(message => message.created_at_ms > after).slice(0, limit);
      const selectedIds = new Set(selected.map(message => message.id));
      const remaining = queue.filter(message => !selectedIds.has(message.id));
      if (remaining.length) mailboxes.set(input.peer_id, remaining); else mailboxes.delete(input.peer_id);
      return json(res, 200, { messages: selected });
    }

    return error(res, 404, 'not found');
  }

  const server = http.createServer((req, res) => handle(req, res).catch(cause => error(res, 500, cause.message)));
  return { server, cleanup, state: { invites, mailboxes, seenMessages, rateBuckets } };
}

if (require.main === module) {
  const port = Number(process.env.PORT || 18081);
  const host = process.env.HOST || '127.0.0.1';
  const { server } = createCoordinator();
  server.listen(port, host, () => console.log(`trusted-carpool coordinator listening on ${host}:${port}`));
}

module.exports = {
  canonicalInvite,
  canonicalMessage,
  canonicalPoll,
  canonicalTurnCredentials,
  createCoordinator,
  desktopDownloadUrls,
  desktopReleaseVersion,
  joinPage,
  peerIdFromPublicKey,
  turnRestCredentials,
  validCode,
  validPeerId,
};
