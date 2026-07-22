import { execFile, spawn } from 'node:child_process';
import { createHash, createSign, generateKeyPairSync } from 'node:crypto';
import { lookup } from 'node:dns/promises';
import { mkdtemp, readFile, rm } from 'node:fs/promises';
import { createServer } from 'node:net';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { promisify } from 'node:util';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const execFileAsync = promisify(execFile);
const chromeCandidates = [
  process.env.CHROME_BIN,
  '/usr/bin/google-chrome',
  '/usr/bin/chromium-browser',
  '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
  '/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge',
].filter(Boolean);
// Pin the TURN host to a concrete address so Chrome's resolver mapping stays
// stable for the whole run. Prefer live DNS; fall back to the last known
// address when DNS fails or returns a proxy fake-IP (198.18.0.0/15), which
// cannot carry TURN UDP traffic.
const FALLBACK_TURN_IP = '192.220.24.20';
const resolveTurnHostIp = async () => {
  if (process.env.TURN_HOST_IP) return process.env.TURN_HOST_IP;
  try {
    const { address } = await lookup('p2p.cnaigc.ai', { family: 4 });
    return /^198\.1[89]\./.test(address) ? FALLBACK_TURN_IP : address;
  } catch {
    return FALLBACK_TURN_IP;
  }
};
const turnHostIp = await resolveTurnHostIp();

const freePort = () =>
  new Promise((resolvePort, reject) => {
    const server = createServer();
    server.once('error', reject);
    server.listen(0, '127.0.0.1', () => {
      const address = server.address();
      const port = typeof address === 'object' && address ? address.port : 0;
      server.close(error => (error ? reject(error) : resolvePort(port)));
    });
  });

const waitFor = async (operation, timeoutMs, label) => {
  const deadline = Date.now() + timeoutMs;
  let lastError;
  while (Date.now() < deadline) {
    try {
      return await operation();
    } catch (error) {
      lastError = error;
      await new Promise(resolveWait => setTimeout(resolveWait, 200));
    }
  }
  throw new Error(`${label}超时: ${lastError ?? 'unknown'}`);
};

const peerIdFromPublicKey = publicKeyBase64 => {
  const bytes = Buffer.from(publicKeyBase64, 'base64');
  const hash = createHash('sha256').update(bytes).digest().subarray(0, 16);
  return `p2p-${hash.toString('base64url')}`;
};

const ephemeralPeerIdentity = () => {
  const { publicKey, privateKey } = generateKeyPairSync('ec', { namedCurve: 'prime256v1' });
  const spki = publicKey.export({ format: 'der', type: 'spki' });
  const raw = spki.subarray(spki.length - 65);
  const publicKeyBase64 = raw.toString('base64');
  return {
    peerId: peerIdFromPublicKey(publicKeyBase64),
    publicKey: publicKeyBase64,
    sign(payload) {
      const signer = createSign('SHA256');
      signer.update(payload);
      signer.end();
      return signer.sign(privateKey).toString('base64');
    },
  };
};

const fetchOfficialTurnCredentials = async () => {
  const peer = ephemeralPeerIdentity();
  const body = {
    peer_id: peer.peerId,
    public_key: peer.publicKey,
    timestamp_ms: Date.now(),
  };
  body.signature = peer.sign(JSON.stringify({
    peer_id: body.peer_id,
    public_key: body.public_key,
    timestamp_ms: body.timestamp_ms,
  }));
  const credentialUrl = 'https://p2p.cnaigc.ai/api/v1/turn-credentials';
  try {
    const { stdout } = await execFileAsync('curl', [
      '--fail',
      '--silent',
      '--show-error',
      '-X', 'POST',
      '-H', 'content-type: application/json',
      '--data-binary', JSON.stringify(body),
      credentialUrl,
    ]);
    return JSON.parse(stdout);
  } catch (postError) {
    // Transition: older coordinators only expose unsigned GET. Remove once
    // the official relay requires signed POST everywhere.
    try {
      const { stdout } = await execFileAsync('curl', [
        '--fail',
        '--silent',
        '--show-error',
        `${credentialUrl}?peer_id=${encodeURIComponent(peer.peerId)}`,
      ]);
      return JSON.parse(stdout);
    } catch {
      throw postError;
    }
  }
};

const connectCdp = async webSocketUrl => {
  const socket = new WebSocket(webSocketUrl);
  await new Promise((resolveOpen, reject) => {
    socket.addEventListener('open', resolveOpen, { once: true });
    socket.addEventListener('error', reject, { once: true });
  });
  let sequence = 0;
  const pending = new Map();
  socket.addEventListener('message', event => {
    const message = JSON.parse(String(event.data));
    if (!message.id) return;
    const handler = pending.get(message.id);
    if (!handler) return;
    pending.delete(message.id);
    if (message.error) handler.reject(new Error(JSON.stringify(message.error)));
    else handler.resolve(message.result);
  });
  return {
    send(method, params = {}) {
      const id = ++sequence;
      return new Promise((resolveResult, reject) => {
        pending.set(id, { resolve: resolveResult, reject });
        socket.send(JSON.stringify({ id, method, params }));
      });
    },
    close() {
      socket.close();
    },
  };
};

let vite;
let chrome;
let cdp;
let profile;
const waitForExit = child =>
  child && child.exitCode === null
    ? new Promise(resolveExit => child.once('exit', resolveExit))
    : Promise.resolve();
try {
  const chromePath = chromeCandidates.find(candidate => candidate && process.getBuiltinModule('node:fs').existsSync(candidate));
  if (!chromePath) throw new Error('没有找到 Chrome 或 Edge');
  const vitePort = await freePort();
  const debugPort = await freePort();
  profile = await mkdtemp(join(tmpdir(), 'trusted-carpool-chrome-'));
  vite = spawn(
    process.execPath,
    [join(root, 'node_modules/vite/bin/vite.js'), '--host', '127.0.0.1', '--port', String(vitePort), '--strictPort'],
    { cwd: root, stdio: ['ignore', 'pipe', 'pipe'] }
  );
  await waitFor(async () => {
    const response = await fetch(`http://127.0.0.1:${vitePort}/e2e/webrtc-harness.html`);
    if (!response.ok) throw new Error(String(response.status));
  }, 20_000, 'Vite');

  const rawTurn = await fetchOfficialTurnCredentials();
  const rawUrls = Array.isArray(rawTurn.urls) ? rawTurn.urls : [rawTurn.urls];
  const turn = {
    ...rawTurn,
    urls: rawUrls.map(url => String(url).replace('p2p.cnaigc.ai', turnHostIp)),
  };
  const ice = Buffer.from(
    JSON.stringify({ urls: turn.urls, username: turn.username, credential: turn.credential })
  ).toString('base64url');
  const pageUrl = `http://127.0.0.1:${vitePort}/e2e/webrtc-harness.html?ice=${ice}`;
  chrome = spawn(
    chromePath,
    [
      '--headless=new',
      '--disable-gpu',
      '--disable-features=WebRtcHideLocalIpsWithMdns',
      '--no-first-run',
      '--no-default-browser-check',
      '--remote-allow-origins=*',
      `--host-resolver-rules=MAP p2p.cnaigc.ai ${turnHostIp}`,
      `--remote-debugging-port=${debugPort}`,
      `--user-data-dir=${profile}`,
      pageUrl,
    ],
    { stdio: ['ignore', 'pipe', 'pipe'] }
  );
  const target = await waitFor(async () => {
    const response = await fetch(`http://127.0.0.1:${debugPort}/json/list`);
    const targets = await response.json();
    const page = targets.find(item => item.type === 'page' && item.url.startsWith(pageUrl));
    if (!page?.webSocketDebuggerUrl) throw new Error('页面目标尚未就绪');
    return page;
  }, 20_000, 'Chrome DevTools');
  cdp = await connectCdp(target.webSocketDebuggerUrl);
  await cdp.send('Runtime.enable');
  const result = await waitFor(async () => {
    const evaluation = await cdp.send('Runtime.evaluate', {
      expression: 'window.__E2E_RESULT__',
      returnByValue: true,
    });
    const value = evaluation.result?.value;
    if (!value || value.status === 'running') throw new Error('WebRTC 测试仍在运行');
    return value;
  }, 240_000, '浏览器 WebRTC E2E');
  process.stdout.write(`${JSON.stringify(result, null, 2)}\n`);
  if (result.status !== 'passed') process.exitCode = 1;
} finally {
  cdp?.close();
  chrome?.kill('SIGTERM');
  vite?.kill('SIGTERM');
  await Promise.all([waitForExit(chrome), waitForExit(vite)]);
  if (profile) {
    await rm(profile, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
  }
}

// Node 22 on macOS can retain an idle platform socket after the built-in
// fetch/WebSocket clients have closed. All owned children and temporary files
// are already awaited above, so end the one-shot verifier deterministically.
process.exit(process.exitCode ?? 0);
