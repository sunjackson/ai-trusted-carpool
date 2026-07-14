import {
  acceptChunkFrame,
  createChunkFrames,
  type Assembly,
  type WireMessage,
} from '../src/webrtcWire';

type CandidateEvidence = {
  localCandidateType: string;
  remoteCandidateType: string;
  protocol: string;
  relayProtocol: string | null;
};

type CaseResult = {
  name: string;
  frameCount: number;
  payloadBytes: number;
  candidate: CandidateEvidence;
};

declare global {
  interface Window {
    __E2E_RESULT__?:
      | { status: 'running' }
      | { status: 'passed'; cases: CaseResult[] }
      | { status: 'failed'; error: string; cases?: CaseResult[] };
  }
}

const resultElement = document.querySelector<HTMLPreElement>('#result');
const show = (value: unknown) => {
  if (resultElement) resultElement.textContent = JSON.stringify(value, null, 2);
};

const timeout = <T>(promise: Promise<T>, milliseconds: number, label: string): Promise<T> =>
  new Promise((resolve, reject) => {
    const timer = window.setTimeout(() => reject(new Error(`${label}超时`)), milliseconds);
    promise.then(
      value => {
        window.clearTimeout(timer);
        resolve(value);
      },
      error => {
        window.clearTimeout(timer);
        reject(error);
      }
    );
  });

const waitForOpen = (channel: RTCDataChannel): Promise<void> => {
  if (channel.readyState === 'open') return Promise.resolve();
  return new Promise((resolve, reject) => {
    channel.addEventListener('open', () => resolve(), { once: true });
    channel.addEventListener('error', () => reject(new Error('数据通道打开失败')), { once: true });
  });
};

const waitForIceGathering = (connection: RTCPeerConnection): Promise<void> => {
  if (connection.iceGatheringState === 'complete') return Promise.resolve();
  return new Promise(resolve => {
    const onState = () => {
      if (connection.iceGatheringState === 'complete') {
        connection.removeEventListener('icegatheringstatechange', onState);
        resolve();
      }
    };
    connection.addEventListener('icegatheringstatechange', onState);
  });
};

const waitForBuffer = (channel: RTCDataChannel): Promise<void> => {
  if (channel.bufferedAmount <= 1024 * 1024) return Promise.resolve();
  channel.bufferedAmountLowThreshold = 512 * 1024;
  return new Promise(resolve => {
    channel.addEventListener('bufferedamountlow', () => resolve(), { once: true });
  });
};

const directDescription = (description: RTCSessionDescription): RTCSessionDescriptionInit => ({
  type: description.type,
  // Clash/TUN exposes RFC 2544 benchmark addresses as host candidates. The underlying UDP socket
  // is still bound on all interfaces, so loopback is the deterministic direct path for local E2E.
  sdp: description.sdp.replace(
    /^(a=candidate:[^\r\n]+ udp [^\r\n]+ )198\.18\.\d+\.\d+( \d+ typ host)/gim,
    '$1127.0.0.1$2'
  ),
});

const connectionEvidence = (connection: RTCPeerConnection) => ({
  connectionState: connection.connectionState,
  iceConnectionState: connection.iceConnectionState,
  iceGatheringState: connection.iceGatheringState,
  signalingState: connection.signalingState,
  candidates: (connection.localDescription?.sdp.match(/^a=candidate:/gm) ?? []).length,
  candidateKinds: (connection.localDescription?.sdp.match(/^a=candidate:.*$/gm) ?? []).map(line => {
    const fields = line.split(/\s+/);
    return {
      protocol: fields[2] ?? 'unknown',
      address: fields[4] ?? 'unknown',
      addressKind: fields[4]?.endsWith('.local') ? 'mdns' : fields[4]?.includes(':') ? 'ipv6' : 'ipv4',
      type: fields[7] ?? 'unknown',
      tcpType: fields.includes('tcptype') ? fields[fields.indexOf('tcptype') + 1] : null,
    };
  }),
});

async function selectedCandidateEvidence(connection: RTCPeerConnection): Promise<CandidateEvidence> {
  for (let attempt = 0; attempt < 40; attempt += 1) {
    const stats = await connection.getStats();
    let selectedPairId: string | undefined;
    stats.forEach(report => {
      if (report.type === 'transport' && report.selectedCandidatePairId) {
        selectedPairId = report.selectedCandidatePairId as string;
      }
    });
    let pair = selectedPairId ? stats.get(selectedPairId) : undefined;
    if (!pair) {
      stats.forEach(report => {
        if (
          report.type === 'candidate-pair' &&
          report.state === 'succeeded' &&
          (report.nominated || !pair)
        ) {
          pair = report;
        }
      });
    }
    if (pair) {
      const local = stats.get(pair.localCandidateId);
      const remote = stats.get(pair.remoteCandidateId);
      if (local && remote) {
        return {
          localCandidateType: String(local.candidateType ?? 'unknown'),
          remoteCandidateType: String(remote.candidateType ?? 'unknown'),
          protocol: String(local.protocol ?? 'unknown'),
          relayProtocol: local.relayProtocol ? String(local.relayProtocol) : null,
        };
      }
    }
    await new Promise(resolve => window.setTimeout(resolve, 100));
  }
  throw new Error('无法读取已选 ICE 候选对');
}

async function runCase(
  name: string,
  iceServers: RTCIceServer[],
  iceTransportPolicy: RTCIceTransportPolicy,
  candidateRequirement: 'relay' | 'non-relay',
  payloadRepeat: number
): Promise<CaseResult> {
  const configuration: RTCConfiguration = { iceServers, iceTransportPolicy };
  const sender = new RTCPeerConnection(configuration);
  const receiver = new RTCPeerConnection(configuration);

  try {
    const outgoing = sender.createDataChannel(`trusted-carpool-${name}`, { ordered: true });
    const incomingPromise = new Promise<RTCDataChannel>(resolve => {
      receiver.addEventListener('datachannel', event => resolve(event.channel), { once: true });
    });
    const offer = await sender.createOffer();
    await sender.setLocalDescription(offer);
    await timeout(waitForIceGathering(sender), 20_000, `${name} 发起方 ICE 收集`);
    const senderDescription = sender.localDescription ?? offer;
    await receiver.setRemoteDescription(
      name === 'direct' && sender.localDescription
        ? directDescription(sender.localDescription)
        : senderDescription
    );
    const answer = await receiver.createAnswer();
    await receiver.setLocalDescription(answer);
    await timeout(waitForIceGathering(receiver), 20_000, `${name} 接收方 ICE 收集`);
    const receiverDescription = receiver.localDescription ?? answer;
    await sender.setRemoteDescription(
      name === 'direct' && receiver.localDescription
        ? directDescription(receiver.localDescription)
        : receiverDescription
    );

    let incoming: RTCDataChannel;
    try {
      incoming = await timeout(incomingPromise, 30_000, `${name} 接收数据通道`);
      await timeout(
        Promise.all([waitForOpen(outgoing), waitForOpen(incoming)]),
        30_000,
        `${name} 连接`
      );
    } catch (error) {
      throw new Error(
        `${error instanceof Error ? error.message : String(error)}; ` +
          `sender=${JSON.stringify(connectionEvidence(sender))}; ` +
          `receiver=${JSON.stringify(connectionEvidence(receiver))}`
      );
    }
    const expected: WireMessage = {
      type: 'relay_request',
      bridgeRequestId: `bridge-${name}`,
      payloadJson: JSON.stringify({
        model: 'gpt-5.6-luna',
        body: `可信拼车-${name}-`.repeat(payloadRepeat),
      }),
    };
    const frames = createChunkFrames(expected, `message-${name}`);
    const assemblies = new Map<string, Assembly>();
    const received = timeout(
      new Promise<WireMessage>((resolve, reject) => {
        incoming.onmessage = event => {
          const message = acceptChunkFrame(assemblies, String(event.data));
          if (message) resolve(message);
        };
        incoming.onerror = () => reject(new Error(`${name} 接收数据失败`));
      }),
      30_000,
      `${name} 大消息传输`
    );
    for (const frame of frames) {
      await waitForBuffer(outgoing);
      outgoing.send(frame);
    }
    const actual = await received;
    if (JSON.stringify(actual) !== JSON.stringify(expected)) {
      throw new Error(`${name} 分片重组内容不一致`);
    }
    const candidate = await selectedCandidateEvidence(sender);
    if (candidateRequirement === 'relay' && candidate.localCandidateType !== 'relay') {
      throw new Error(`${name} 没有使用 TURN relay 候选`);
    }
    if (candidateRequirement === 'non-relay' && candidate.localCandidateType === 'relay') {
      throw new Error(`${name} 只建立了 TURN 中继，没有建立直连`);
    }
    return {
      name,
      frameCount: frames.length,
      payloadBytes: new TextEncoder().encode(expected.payloadJson).length,
      candidate,
    };
  } finally {
    sender.close();
    receiver.close();
  }
}

const decodeIceServers = (): RTCIceServer => {
  const encoded = new URLSearchParams(window.location.search).get('ice');
  if (!encoded) throw new Error('缺少 TURN 测试凭据');
  const base64 = encoded.replace(/-/g, '+').replace(/_/g, '/');
  const padded = base64.padEnd(Math.ceil(base64.length / 4) * 4, '=');
  return JSON.parse(new TextDecoder().decode(Uint8Array.from(atob(padded), value => value.charCodeAt(0)))) as RTCIceServer;
};

async function main() {
  window.__E2E_RESULT__ = { status: 'running' };
  show(window.__E2E_RESULT__);
  const turn = decodeIceServers();
  const urls = Array.isArray(turn.urls) ? turn.urls : [turn.urls];
  const udp = { ...turn, urls: urls.filter(url => String(url).includes('transport=udp')) };
  const tcp = { ...turn, urls: urls.filter(url => String(url).includes('transport=tcp')) };
  const stun: RTCIceServer = {
    urls: urls
      .filter(url => String(url).includes('transport=udp'))
      .map(url => String(url).replace(/^turn:/, 'stun:').replace(/\?transport=udp$/, '')),
  };
  const definitions: Array<Parameters<typeof runCase>> = [
    ['direct', [stun], 'all', 'non-relay', 3_000],
    ['turn-udp', [udp], 'relay', 'relay', 3_000],
    ['turn-tcp', [tcp], 'relay', 'relay', 15_000],
  ];
  const cases: CaseResult[] = [];
  const errors: string[] = [];
  for (const definition of definitions) {
    try {
      cases.push(await runCase(...definition));
    } catch (error) {
      errors.push(error instanceof Error ? error.message : String(error));
    }
  }
  if (errors.length > 0) {
    window.__E2E_RESULT__ = { status: 'failed', error: errors.join('\n'), cases };
    show(window.__E2E_RESULT__);
    return;
  }
  window.__E2E_RESULT__ = { status: 'passed', cases };
  show(window.__E2E_RESULT__);
}

void main().catch(error => {
  window.__E2E_RESULT__ = {
    status: 'failed',
    error: error instanceof Error ? `${error.message}\n${error.stack ?? ''}` : String(error),
  };
  show(window.__E2E_RESULT__);
});
