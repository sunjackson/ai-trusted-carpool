'use strict';

const assert = require('node:assert/strict');
const crypto = require('node:crypto');
const http = require('node:http');
const test = require('node:test');
const {
  canonicalInvite,
  canonicalMessage,
  canonicalPoll,
  canonicalTurnCredentials,
  createCoordinator,
  desktopDownloadUrls,
  desktopReleaseVersion,
  joinPage,
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
    const nonce = join.text.match(/<script nonce="([^"]+)">/)?.[1];
    assert.ok(nonce);
    assert.ok(join.headers['content-security-policy'].includes(`script-src 'nonce-${nonce}'`));
    assert.doesNotMatch(join.headers['content-security-policy'], /script-src 'unsafe-inline'/);
    assert.match(join.text, new RegExp(`trusted-carpool://join/${invite.code}`));
    assert.doesNotMatch(join.text, /http-equiv="refresh"/);
    assert.match(join.text, /navigator\.userAgentData/);
    assert.match(join.text, /visibilitychange/);
    assert.match(join.text, /Trusted-Carpool_0\.0\.7_x64-setup\.exe/);
    assert.match(join.text, /Trusted-Carpool_0\.0\.7_universal\.dmg/);
    assert.match(join.text, /Trusted-Carpool_0\.0\.7_amd64\.AppImage/);
    assert.match(join.text, /Trusted-Carpool_0\.0\.7_amd64\.deb/);
    assert.match(join.text, /SHA256SUMS\.txt/);
    assert.doesNotMatch(join.text, /owner_public_key|payload_base64|signature/);
  });
});

test('join download recommendations stay on the pinned project release', () => {
  assert.equal(desktopReleaseVersion('1.2.3'), '1.2.3');
  assert.equal(desktopReleaseVersion('../latest'), '0.0.7');
  const downloads = desktopDownloadUrls('1.2.3');
  assert.equal(
    downloads.windows,
    'https://github.com/sunjackson/ai-trusted-carpool/releases/download/v1.2.3/Trusted-Carpool_1.2.3_x64-setup.exe'
  );
  assert.match(downloads.macos, /Trusted-Carpool_1\.2\.3_universal\.dmg$/);
  assert.match(downloads.appImage, /Trusted-Carpool_1\.2\.3_amd64\.AppImage$/);
  assert.match(downloads.deb, /Trusted-Carpool_1\.2\.3_amd64\.deb$/);
  assert.throws(() => joinPage('<script>alert(1)</script>'), /invalid join code/);
  assert.throws(
    () => joinPage('7G2K5LQ8M4TZ', { scriptNonce: 'too-short' }),
    /invalid script nonce/
  );
});

test('renewed short invite leases stay online and expire after the host stops renewing', async () => {
  let now = 1_800_000_000_000;
  await withServer(async base => {
    const owner = identity();
    const invite = {
      code: 'M9Q3TP7W6KXR',
      owner_peer_id: owner.peerId,
      owner_public_key: owner.publicKey,
      owner_encryption_public_key: owner.encryptionPublicKey,
      car_id: crypto.randomUUID(),
      seat_no: 1,
      payload_base64: Buffer.from(JSON.stringify({ always_on: true, expires_at_ms: Number.MAX_SAFE_INTEGER })).toString('base64'),
      expires_at_ms: now + 180_000,
      timestamp_ms: now,
    };
    invite.signature = owner.sign(canonicalInvite(invite));
    assert.equal((await request(`${base}/api/v1/carpool/invites`, 'POST', invite)).status, 200);

    now += 60_000;
    invite.expires_at_ms = now + 180_000;
    invite.timestamp_ms = now;
    invite.signature = owner.sign(canonicalInvite(invite));
    assert.equal((await request(`${base}/api/v1/carpool/invites`, 'POST', invite)).status, 200);

    now += 179_000;
    assert.equal((await request(`${base}/api/v1/carpool/invites/${invite.code}`)).status, 200);
    now += 2_000;
    assert.equal((await request(`${base}/api/v1/carpool/invites/${invite.code}`)).status, 404);
  }, { clock: () => now });
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

test('issues verifiable time-limited turn credentials only with a peer proof', async () => {
  const secret = 'test-shared-turn-secret';
  const urls = ['turn:relay.example.com:3478?transport=udp', 'turns:relay.example.com:5349'];
  await withServer(async base => {
    const peer = identity();
    const before = Math.floor(Date.now() / 1000);
    const requestBody = {
      peer_id: peer.peerId,
      public_key: peer.publicKey,
      timestamp_ms: Date.now(),
    };
    requestBody.signature = peer.sign(canonicalTurnCredentials(requestBody));
    const response = await request(`${base}/api/v1/turn-credentials`, 'POST', requestBody);
    assert.equal(response.status, 200);
    assert.deepEqual(response.body.urls, urls);
    assert.equal(response.body.ttl_seconds, 120);

    const [expiry, embeddedPeerId] = response.body.username.split(':');
    assert.equal(embeddedPeerId, peer.peerId);
    const expiresAt = Number(expiry);
    assert.ok(expiresAt >= before + 120 && expiresAt <= before + 121);

    const expected = crypto
      .createHmac('sha1', secret)
      .update(response.body.username)
      .digest('base64');
    assert.equal(response.body.credential, expected);

    assert.equal((await request(`${base}/api/v1/turn-credentials?peer_id=${encodeURIComponent(peer.peerId)}`)).status, 405);
  }, { turnSecret: secret, turnUrls: urls, turnTtlSeconds: 120 });
});

test('rejects turn credential requests without a valid peer proof', async () => {
  await withServer(async base => {
    const peer = identity();
    const missing = await request(`${base}/api/v1/turn-credentials`, 'POST', {});
    assert.equal(missing.status, 400);
    const malformed = await request(`${base}/api/v1/turn-credentials`, 'POST', {
      peer_id: 'not-a-peer',
      public_key: peer.publicKey,
      timestamp_ms: Date.now(),
      signature: 'aa',
    });
    assert.equal(malformed.status, 400);
    const unsigned = {
      peer_id: peer.peerId,
      public_key: peer.publicKey,
      timestamp_ms: Date.now(),
      signature: 'not-a-signature',
    };
    assert.equal((await request(`${base}/api/v1/turn-credentials`, 'POST', unsigned)).status, 400);
  }, { turnSecret: 'test-shared-turn-secret', turnUrls: ['turn:relay.example.com:3478?transport=udp'] });
});

test('reports turn relay as unconfigured without a shared secret', async () => {
  await withServer(async base => {
    const peer = identity();
    const body = {
      peer_id: peer.peerId,
      public_key: peer.publicKey,
      timestamp_ms: Date.now(),
    };
    body.signature = peer.sign(canonicalTurnCredentials(body));
    const response = await request(`${base}/api/v1/turn-credentials`, 'POST', body);
    assert.equal(response.status, 404);
    assert.match(response.body.error, /not configured/);
  }, { turnSecret: '', turnUrls: [] });
});

test('rate limits turn credential requests by source address', async () => {
  await withServer(async base => {
    const peer = identity();
    async function signedTurn() {
      const body = {
        peer_id: peer.peerId,
        public_key: peer.publicKey,
        timestamp_ms: Date.now(),
      };
      body.signature = peer.sign(canonicalTurnCredentials(body));
      return request(`${base}/api/v1/turn-credentials`, 'POST', body);
    }
    assert.equal((await signedTurn()).status, 200);
    assert.equal((await signedTurn()).status, 200);
    const limited = await signedTurn();
    assert.equal(limited.status, 429);
    assert.match(limited.body.error, /too many/);
  }, {
    turnRateLimit: 2,
    turnSecret: 'test-shared-turn-secret',
    turnUrls: ['turn:relay.example.com:3478?transport=udp'],
  });
});

test('rate limits invite registration and enforces per-owner quota', async () => {
  await withServer(async base => {
    const owner = identity();
    const now = Date.now();
    async function register(code, seatNo) {
      const invite = {
        code,
        owner_peer_id: owner.peerId,
        owner_public_key: owner.publicKey,
        owner_encryption_public_key: owner.encryptionPublicKey,
        car_id: crypto.randomUUID(),
        seat_no: seatNo,
        payload_base64: Buffer.from(JSON.stringify({ car_name: '配额测试' })).toString('base64'),
        expires_at_ms: now + 60_000,
        timestamp_ms: now,
      };
      invite.signature = owner.sign(canonicalInvite(invite));
      return request(`${base}/api/v1/carpool/invites`, 'POST', invite);
    }
    assert.equal((await register('7G2K5LQ8M4TZ', 1)).status, 200);
    assert.equal((await register('M9Q3TP7W6KXR', 2)).status, 200);
    const limited = await register('ABCD2345EFGH', 3);
    assert.equal(limited.status, 429);
    assert.match(limited.body.error, /too many invite registrations/);
  }, { registerRateLimit: 2, maxInvitesPerOwner: 16 });
});

test('rejects more than the configured active invites per owner', async () => {
  await withServer(async base => {
    const owner = identity();
    const now = Date.now();
    const codes = ['7G2K5LQ8M4TZ', 'M9Q3TP7W6KXR', 'ABCD2345EFGH'];
    for (let index = 0; index < codes.length; index += 1) {
      const invite = {
        code: codes[index],
        owner_peer_id: owner.peerId,
        owner_public_key: owner.publicKey,
        owner_encryption_public_key: owner.encryptionPublicKey,
        car_id: crypto.randomUUID(),
        seat_no: (index % 4) + 1,
        payload_base64: Buffer.from(JSON.stringify({ car_name: '名额测试' })).toString('base64'),
        expires_at_ms: now + 60_000,
        timestamp_ms: now,
      };
      invite.signature = owner.sign(canonicalInvite(invite));
      const response = await request(`${base}/api/v1/carpool/invites`, 'POST', invite);
      if (index < 2) assert.equal(response.status, 200);
      else {
        assert.equal(response.status, 429);
        assert.match(response.body.error, /owner invite quota/);
      }
    }
  }, { maxInvitesPerOwner: 2, registerRateLimit: 100 });
});

test('allows concurrent polls from different peers behind one IP', async () => {
  await withServer(async base => {
    const host = identity();
    const passenger = identity();
    async function poll(peer) {
      const body = {
        peer_id: peer.peerId,
        public_key: peer.publicKey,
        after_ms: null,
        limit: 10,
        timestamp_ms: Date.now(),
      };
      body.signature = peer.sign(canonicalPoll(body));
      return request(`${base}/api/v1/carpool/messages/poll`, 'POST', body);
    }
    for (let i = 0; i < 5; i += 1) {
      assert.equal((await poll(host)).status, 200);
      assert.equal((await poll(passenger)).status, 200);
    }
  }, { pollRateLimit: 12, pollPeerRateLimit: 6 });
});

test('rate limits outbound messages by source address', async () => {
  await withServer(async base => {
    const passenger = identity();
    const owner = identity();
    async function send(kindSuffix) {
      const message = {
        from_peer_id: passenger.peerId,
        to_peer_id: owner.peerId,
        public_key: passenger.publicKey,
        kind: 'carpool_claim',
        payload_json: JSON.stringify({ code: '7G2K5LQ8M4TZ', nickname: kindSuffix }),
        ttl_ms: 60_000,
        timestamp_ms: Date.now(),
      };
      message.signature = passenger.sign(canonicalMessage(message));
      return request(`${base}/api/v1/carpool/messages`, 'POST', message);
    }
    assert.equal((await send('a')).status, 200);
    assert.equal((await send('b')).status, 200);
    const limited = await send('c');
    assert.equal(limited.status, 429);
    assert.match(limited.body.error, /too many messages/);
  }, { messageRateLimit: 2, messagePeerRateLimit: 100 });
});
