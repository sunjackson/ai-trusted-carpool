'use strict';

const crypto = require('crypto');
const http = require('http');
const { URL } = require('url');

const DEFAULT_MAX_TTL_MS = 120_000;
const DEFAULT_INVITE_TTL_MS = 24 * 60 * 60 * 1000;
const MAX_BODY_BYTES = 96 * 1024;
const MAX_INVITES = 20_000;
const MAX_MESSAGES_PER_PEER = 128;
const DEFAULT_RESOLVE_RATE_LIMIT = 60;
const RATE_WINDOW_MS = 60_000;
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

function createCoordinator(options = {}) {
  const invites = new Map();
  const mailboxes = new Map();
  const seenMessages = new Map();
  const rateBuckets = new Map();
  const clock = options.clock || nowMs;
  const trustProxy = options.trustProxy ?? process.env.TRUSTED_CARPOOL_TRUST_PROXY === '1';
  const resolveRateLimit = options.resolveRateLimit ?? DEFAULT_RESOLVE_RATE_LIMIT;

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

    if (req.method === 'POST' && url.pathname === '/api/v1/carpool/invites') {
      let input;
      try { input = await readJson(req); } catch (cause) { return error(res, cause.statusCode || 400, cause.message); }
      const validation = validateInvite(input, clock());
      if (validation) return error(res, 400, validation);
      const existing = invites.get(input.code);
      if (existing && existing.owner_peer_id !== input.owner_peer_id) return error(res, 409, 'code collision');
      if (!existing && invites.size >= MAX_INVITES) return error(res, 503, 'invite capacity reached');
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
      let input;
      try { input = await readJson(req); } catch (cause) { return error(res, cause.statusCode || 400, cause.message); }
      const validation = validateMessage(input, clock());
      if (validation) return error(res, 400, validation);
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
  createCoordinator,
  peerIdFromPublicKey,
  validCode,
  validPeerId,
};
