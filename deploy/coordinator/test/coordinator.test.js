'use strict';

const assert = require('node:assert/strict');
const crypto = require('node:crypto');
const http = require('node:http');
const test = require('node:test');
const {
  canonicalInvite,
  canonicalMessage,
  canonicalPoll,
  createCoordinator,
  peerIdFromPublicKey,
} = require('../server');

function identity() {
  const { publicKey, privateKey } = crypto.generateKeyPairSync('ec', { namedCurve: 'prime256v1' });
  const spki = publicKey.export({ format: 'der', type: 'spki' });
  const raw = spki.subarray(spki.length - 65);
  const publicKeyBase64 = raw.toString('base64');
  return {
    peerId: peerIdFromPublicKey(publicKeyBase64),
    publicKey: publicKeyBase64,
    encryptionPublicKey: crypto.randomBytes(32).toString('base64'),
    sign(payload) {
      const signer = crypto.createSign('SHA256');
      signer.update(payload);
      signer.end();
      return signer.sign(privateKey).toString('base64');
    },
  };
}

async function withServer(run, options) {
  const { server } = createCoordinator(options);
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  const { port } = server.address();
  try { await run(`http://127.0.0.1:${port}`); }
  finally { await new Promise(resolve => server.close(resolve)); }
}

async function request(url, method = 'GET', body) {
  return new Promise((resolve, reject) => {
    const target = new URL(url);
    const req = http.request({ hostname: target.hostname, port: target.port, path: `${target.pathname}${target.search}`, method, headers: body ? { 'content-type': 'application/json' } : {} }, res => {
      const chunks = [];
      res.on('data', chunk => chunks.push(chunk));
      res.on('end', () => {
        const text = Buffer.concat(chunks).toString('utf8');
        const contentType = String(res.headers['content-type'] || '');
        resolve({
          status: res.statusCode,
          headers: res.headers,
          text,
          body: contentType.includes('application/json') ? JSON.parse(text) : text,
        });
      });
    });
    req.on('error', reject);
    if (body) req.write(JSON.stringify(body));
    req.end();
  });
}

test('registers and resolves an owner-signed public invite', async () => {
  await withServer(async base => {
    const owner = identity();
    const now = Date.now();
    const invite = {
      code: '7G2K5LQ8M4TZ',
      owner_peer_id: owner.peerId,
      owner_public_key: owner.publicKey,
      owner_encryption_public_key: owner.encryptionPublicKey,
      car_id: crypto.randomUUID(),
      seat_no: 1,
      payload_base64: Buffer.from(JSON.stringify({ car_name: '测试车队' })).toString('base64'),
      expires_at_ms: now + 60_000,
      timestamp_ms: now,
    };
    invite.signature = owner.sign(canonicalInvite(invite));
    const registered = await request(`${base}/api/v1/carpool/invites`, 'POST', invite);
    assert.equal(registered.status, 200);
    const resolved = await request(`${base}/api/v1/carpool/invites/${invite.code}`);
    assert.equal(resolved.status, 200);
    assert.equal(resolved.body.invite.owner_peer_id, owner.peerId);
    const join = await request(`${base}/api/v1/carpool/join/${invite.code}`);
    assert.equal(join.status, 200);
    assert.match(join.headers['content-type'], /^text\/html/);
    assert.match(join.headers['content-security-policy'], /default-src 'none'/);
    assert.match(join.text, new RegExp(`trusted-carpool://join/${invite.code}`));
    assert.doesNotMatch(join.text, /owner_public_key|payload_base64|signature/);
  });
});

test('does not create launch pages for unknown or malformed invite codes', async () => {
  await withServer(async base => {
    assert.equal((await request(`${base}/api/v1/carpool/join/7G2K5LQ8M4TZ`)).status, 404);
    assert.equal((await request(`${base}/api/v1/carpool/join/%3Cscript%3E`)).status, 404);
  });
});

test('rejects tampered invite metadata', async () => {
  await withServer(async base => {
    const owner = identity();
    const now = Date.now();
    const invite = {
      code: 'M9Q3TP7W6KXR', owner_peer_id: owner.peerId, owner_public_key: owner.publicKey,
      owner_encryption_public_key: owner.encryptionPublicKey, car_id: crypto.randomUUID(), seat_no: 2,
      payload_base64: Buffer.from(JSON.stringify({ car_name: '不可篡改的测试车队' })).toString('base64'), expires_at_ms: now + 60_000, timestamp_ms: now,
    };
    invite.signature = owner.sign(canonicalInvite(invite));
    invite.seat_no = 3;
    const response = await request(`${base}/api/v1/carpool/invites`, 'POST', invite);
    assert.equal(response.status, 400);
    assert.match(response.body.error, /signature/);
  });
});

test('delivers signed claim messages once', async () => {
  await withServer(async base => {
    const passenger = identity();
    const owner = identity();
    const now = Date.now();
    const message = {
      from_peer_id: passenger.peerId,
      to_peer_id: owner.peerId,
      public_key: passenger.publicKey,
      kind: 'carpool_claim',
      payload_json: JSON.stringify({ code: '7G2K5LQ8M4TZ', nickname: '小雨' }),
      ttl_ms: 60_000,
      timestamp_ms: now,
    };
    message.signature = passenger.sign(canonicalMessage(message));
    assert.equal((await request(`${base}/api/v1/carpool/messages`, 'POST', message)).status, 200);
    assert.equal((await request(`${base}/api/v1/carpool/messages`, 'POST', message)).status, 409);

    const poll = { peer_id: owner.peerId, public_key: owner.publicKey, after_ms: null, limit: 10, timestamp_ms: Date.now() };
    poll.signature = owner.sign(canonicalPoll(poll));
    const first = await request(`${base}/api/v1/carpool/messages/poll`, 'POST', poll);
    assert.equal(first.status, 200);
    assert.equal(first.body.messages.length, 1);
    assert.equal(first.body.messages[0].ttl_ms, 60_000);
    assert.equal(first.body.messages[0].public_key, passenger.publicKey);
    assert.equal(first.body.messages[0].signature, message.signature);
    poll.timestamp_ms = Date.now();
    poll.signature = owner.sign(canonicalPoll(poll));
    const second = await request(`${base}/api/v1/carpool/messages/poll`, 'POST', poll);
    assert.equal(second.body.messages.length, 0);
  });
});

test('rate limits invite enumeration by source address', async () => {
  await withServer(async base => {
    assert.equal((await request(`${base}/api/v1/carpool/invites/7G2K5LQ8M4TZ`)).status, 404);
    assert.equal((await request(`${base}/api/v1/carpool/invites/M9Q3TP7W6KXR`)).status, 404);
    const limited = await request(`${base}/api/v1/carpool/invites/ABCD2345EFGH`);
    assert.equal(limited.status, 429);
    assert.match(limited.body.error, /too many/);
  }, { resolveRateLimit: 2 });
});
