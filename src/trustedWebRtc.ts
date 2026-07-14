import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import type {
  CoordinatorMessage,
  IceServer,
  RelayBridgeRequestEvent,
  RelayRequest,
  RelayStreamEvent,
  RideAccess,
} from './types';
import {
  acceptChunkFrame,
  createChunkFrames,
  type Assembly,
  type WireMessage,
} from './webrtcWire';

type Role = 'host' | 'passenger';
type SignalPayload = {
  sdp?: RTCSessionDescriptionInit;
  candidate?: RTCIceCandidateInit;
};
type Connection = {
  peerId: string;
  pc: RTCPeerConnection;
  channel: RTCDataChannel | null;
  pendingCandidates: RTCIceCandidateInit[];
  assemblies: Map<string, Assembly>;
  receiveChain: Promise<void>;
  sendChain: Promise<void>;
};

const inTauri = (): boolean => '__TAURI_INTERNALS__' in window;
const MAX_BUFFERED_AMOUNT = 1024 * 1024;

const errorMessage = (error: unknown): string => {
  if (error instanceof Error) return error.message;
  if (typeof error === 'string') return error;
  try {
    return JSON.stringify(error);
  } catch {
    return String(error);
  }
};

class TrustedWebRtcRuntime {
  private role: Role | null = null;
  private accessId: string | null = null;
  private ownerPeerId: string | null = null;
  private connections = new Map<string, Connection>();
  private iceServers: RTCIceServer[] = [];
  private pollTimer: number | null = null;
  private pollBusy = false;
  private unlisten: UnlistenFn[] = [];
  private pendingBridge = new Map<string, RelayBridgeRequestEvent>();
  private hostRequests = new Map<string, Connection>();

  async initialize(): Promise<void> {
    if (!inTauri() || this.unlisten.length > 0) return;
    const requestUnlisten = await listen<RelayBridgeRequestEvent>(
      'trusted-carpool:relay-request',
      event => void this.handleBridgeRequest(event.payload)
    );
    const streamUnlisten = await listen<RelayStreamEvent>(
      'trusted-carpool:relay-stream-event',
      event => void this.handleHostStreamEvent(event.payload)
    );
    this.unlisten = [requestUnlisten, streamUnlisten];
  }

  async startHost(): Promise<void> {
    if (!inTauri()) return;
    await this.initialize();
    this.closeConnections();
    this.role = 'host';
    this.accessId = null;
    this.ownerPeerId = null;
    await this.loadIceServers();
    this.startPolling();
  }

  async startPassenger(access: RideAccess): Promise<void> {
    if (!inTauri()) return;
    if (typeof RTCPeerConnection === 'undefined') {
      throw new Error('当前系统 WebView 不支持 WebRTC，无法安全上车');
    }
    await this.initialize();
    this.closeConnections();
    this.role = 'passenger';
    this.accessId = access.accessId;
    this.ownerPeerId = access.ownerPeerId;
    await this.loadIceServers();
    this.startPolling();
    const connection = this.createConnection(access.ownerPeerId);
    const channel = connection.pc.createDataChannel('trusted-carpool-v1', { ordered: true });
    this.bindChannel(connection, channel);
    const offer = await connection.pc.createOffer();
    await connection.pc.setLocalDescription(offer);
    await this.sendSignal(access.ownerPeerId, 'webrtc_offer', {
      sdp: connection.pc.localDescription ?? offer,
    });
    await this.waitForOpen(connection, 20_000);
  }

  async stop(): Promise<void> {
    if (!inTauri()) return;
    const peers = [...this.connections.keys()];
    await Promise.allSettled(
      peers.map(peerId => this.sendSignal(peerId, 'hangup', {}))
    );
    this.closeConnections();
    this.role = null;
    this.accessId = null;
    this.ownerPeerId = null;
  }

  private async loadIceServers(): Promise<void> {
    const servers = await invoke<IceServer[]>('get_ice_servers');
    this.iceServers = servers.map(server => ({
      urls: server.urls,
      username: server.username ?? undefined,
      credential: server.credential ?? undefined,
    }));
  }

  private closeConnections(): void {
    if (this.pollTimer !== null) window.clearInterval(this.pollTimer);
    this.pollTimer = null;
    for (const connection of this.connections.values()) {
      connection.channel?.close();
      connection.pc.close();
    }
    this.connections.clear();
    this.pendingBridge.clear();
    this.hostRequests.clear();
  }

  private createConnection(peerId: string): Connection {
    const existing = this.connections.get(peerId);
    if (existing) return existing;
    const limit = this.role === 'host' ? 4 : 1;
    if (this.connections.size >= limit) {
      throw new Error(`连接人数已达上限 ${limit}`);
    }
    const pc = new RTCPeerConnection({ iceServers: this.iceServers });
    const connection: Connection = {
      peerId,
      pc,
      channel: null,
      pendingCandidates: [],
      assemblies: new Map(),
      receiveChain: Promise.resolve(),
      sendChain: Promise.resolve(),
    };
    this.connections.set(peerId, connection);
    pc.onicecandidate = event => {
      if (event.candidate) {
        void this.sendSignal(peerId, 'ice_candidate', {
          candidate: event.candidate.toJSON(),
        }).catch(() => undefined);
      }
    };
    pc.ondatachannel = event => this.bindChannel(connection, event.channel);
    pc.onconnectionstatechange = () => {
      if (['failed', 'closed'].includes(pc.connectionState)) {
        connection.channel?.close();
        this.connections.delete(peerId);
      }
    };
    return connection;
  }

  private bindChannel(connection: Connection, channel: RTCDataChannel): void {
    connection.channel = channel;
    channel.binaryType = 'arraybuffer';
    channel.bufferedAmountLowThreshold = MAX_BUFFERED_AMOUNT / 2;
    channel.onopen = () => {
      for (const event of [...this.pendingBridge.values()]) {
        if (event.ownerPeerId === connection.peerId) {
          this.pendingBridge.delete(event.requestId);
          void this.queueWire(connection, {
            type: 'relay_request',
            bridgeRequestId: event.requestId,
            payloadJson: event.payloadJson,
          }).catch(error => void this.failBridge(event.requestId, errorMessage(error)));
        }
      }
    };
    channel.onmessage = event => {
      const raw = String(event.data);
      connection.receiveChain = connection.receiveChain
        .then(() => this.handleFrame(connection, raw))
        .catch(() => undefined);
    };
    channel.onclose = () => {
      if (connection.pc.connectionState === 'closed') {
        this.connections.delete(connection.peerId);
      }
    };
  }

  private waitForOpen(connection: Connection, timeoutMs: number): Promise<void> {
    if (connection.channel?.readyState === 'open') return Promise.resolve();
    return new Promise((resolve, reject) => {
      const deadline = window.setTimeout(() => {
        cleanup();
        reject(new Error('连接车主超时，请确认车主应用保持打开'));
      }, timeoutMs);
      const check = window.setInterval(() => {
        if (connection.channel?.readyState === 'open') {
          cleanup();
          resolve();
        } else if (['failed', 'closed'].includes(connection.pc.connectionState)) {
          cleanup();
          reject(new Error('无法建立安全连接'));
        }
      }, 100);
      const cleanup = () => {
        window.clearTimeout(deadline);
        window.clearInterval(check);
      };
    });
  }

  private async waitForBuffer(channel: RTCDataChannel): Promise<void> {
    if (channel.bufferedAmount <= MAX_BUFFERED_AMOUNT) return;
    await new Promise<void>((resolve, reject) => {
      const timeout = window.setTimeout(() => {
        channel.removeEventListener('bufferedamountlow', onLow);
        reject(new Error('安全连接发送缓冲区超时'));
      }, 10_000);
      const onLow = () => {
        window.clearTimeout(timeout);
        resolve();
      };
      channel.addEventListener('bufferedamountlow', onLow, { once: true });
    });
  }

  private async sendWire(connection: Connection, message: WireMessage): Promise<void> {
    const channel = connection.channel;
    if (!channel || channel.readyState !== 'open') throw new Error('安全连接尚未就绪');
    for (const frame of createChunkFrames(message)) {
      await this.waitForBuffer(channel);
      channel.send(frame);
    }
  }

  private queueWire(connection: Connection, message: WireMessage): Promise<void> {
    const sending = connection.sendChain.then(() => this.sendWire(connection, message));
    connection.sendChain = sending.catch(() => undefined);
    return sending;
  }

  private async handleFrame(connection: Connection, raw: string): Promise<void> {
    const message = acceptChunkFrame(connection.assemblies, raw);
    if (message) await this.handleWireMessage(connection, message);
  }

  private async handleWireMessage(connection: Connection, message: WireMessage): Promise<void> {
    if (message.type === 'relay_request' && this.role === 'host') {
      let request: RelayRequest | null = null;
      try {
        request = JSON.parse(message.payloadJson) as RelayRequest;
        if (!request.requestId || request.requestId !== message.bridgeRequestId) {
          throw new Error('中转请求编号不匹配');
        }
        this.hostRequests.set(request.requestId, connection);
        await invoke<boolean>('start_relay_request', { request });
      } catch (error) {
        if (request?.requestId) this.hostRequests.delete(request.requestId);
        await this.queueWire(connection, {
          type: 'relay_stream_event',
          bridgeRequestId: message.bridgeRequestId,
          payloadJson: JSON.stringify({
            requestId: message.bridgeRequestId,
            kind: 'error',
            error: errorMessage(error),
          } satisfies RelayStreamEvent),
        });
      }
      return;
    }
    if (message.type === 'relay_stream_event' && this.role === 'passenger') {
      const event = JSON.parse(message.payloadJson) as RelayStreamEvent;
      if (event.requestId !== message.bridgeRequestId) {
        throw new Error('流式响应编号不匹配');
      }
      await invoke<boolean>('submit_relay_stream_event', { event });
      return;
    }
    if (message.type === 'relay_response' && this.role === 'passenger') {
      await invoke<boolean>('submit_relay_response', {
        requestId: message.bridgeRequestId,
        payloadJson: message.payloadJson,
      });
    }
  }

  private async handleHostStreamEvent(event: RelayStreamEvent): Promise<void> {
    if (this.role !== 'host') return;
    const connection = this.hostRequests.get(event.requestId);
    if (!connection) return;
    try {
      await this.queueWire(connection, {
        type: 'relay_stream_event',
        bridgeRequestId: event.requestId,
        payloadJson: JSON.stringify(event),
      });
    } finally {
      if (event.kind === 'end' || event.kind === 'error') {
        this.hostRequests.delete(event.requestId);
      }
    }
  }

  private async sendSignal(
    toPeerId: string,
    kind: CoordinatorMessage['kind'],
    payload: SignalPayload
  ): Promise<void> {
    await invoke('send_webrtc_signal', {
      input: { toPeerId, kind, payloadJson: JSON.stringify(payload) },
    });
  }

  private async handleSignal(message: CoordinatorMessage): Promise<void> {
    let payload: SignalPayload;
    try {
      payload = JSON.parse(message.payloadJson) as SignalPayload;
    } catch {
      return;
    }
    if (message.kind === 'hangup') {
      const connection = this.connections.get(message.fromPeerId);
      connection?.channel?.close();
      connection?.pc.close();
      this.connections.delete(message.fromPeerId);
      return;
    }
    if (this.role === 'host' && message.kind === 'webrtc_offer' && payload.sdp) {
      if (!this.connections.has(message.fromPeerId) && this.connections.size >= 4) {
        await this.sendSignal(message.fromPeerId, 'hangup', {});
        return;
      }
      const connection = this.createConnection(message.fromPeerId);
      await connection.pc.setRemoteDescription(payload.sdp);
      await this.flushCandidates(connection);
      const answer = await connection.pc.createAnswer();
      await connection.pc.setLocalDescription(answer);
      await this.sendSignal(message.fromPeerId, 'webrtc_answer', {
        sdp: connection.pc.localDescription ?? answer,
      });
      return;
    }
    if (this.role === 'passenger' && message.kind === 'webrtc_answer' && payload.sdp) {
      const connection = this.connections.get(message.fromPeerId);
      if (!connection) return;
      await connection.pc.setRemoteDescription(payload.sdp);
      await this.flushCandidates(connection);
      return;
    }
    if (message.kind === 'ice_candidate' && payload.candidate) {
      const connection = this.connections.get(message.fromPeerId);
      if (!connection) return;
      if (connection.pc.remoteDescription) {
        await connection.pc.addIceCandidate(payload.candidate);
      } else {
        connection.pendingCandidates.push(payload.candidate);
      }
    }
  }

  private async flushCandidates(connection: Connection): Promise<void> {
    for (const candidate of connection.pendingCandidates.splice(0)) {
      await connection.pc.addIceCandidate(candidate);
    }
  }

  private startPolling(): void {
    if (this.pollTimer !== null) window.clearInterval(this.pollTimer);
    const poll = async () => {
      if (this.pollBusy || !this.role) return;
      this.pollBusy = true;
      try {
        const messages = await invoke<CoordinatorMessage[]>('poll_webrtc_signals', {
          accessId: this.role === 'passenger' ? this.accessId : null,
        });
        for (const message of messages) {
          await this.handleSignal(message).catch(() => undefined);
        }
      } catch {
        // Polling keeps retrying; passenger connection setup has its own visible timeout.
      } finally {
        this.pollBusy = false;
      }
    };
    void poll();
    this.pollTimer = window.setInterval(() => void poll(), 700);
  }

  private async handleBridgeRequest(event: RelayBridgeRequestEvent): Promise<void> {
    if (this.role !== 'passenger' || event.accessId !== this.accessId) {
      await this.failBridge(event.requestId, '当前上车会话没有可用的安全连接');
      return;
    }
    const connection = this.connections.get(event.ownerPeerId);
    if (!connection || connection.channel?.readyState !== 'open') {
      this.pendingBridge.set(event.requestId, event);
      window.setTimeout(() => {
        if (this.pendingBridge.delete(event.requestId)) {
          void this.failBridge(event.requestId, '安全连接尚未就绪，请确认车主保持在线');
        }
      }, Math.min(event.timeoutMs, 15_000));
      return;
    }
    try {
      await this.queueWire(connection, {
        type: 'relay_request',
        bridgeRequestId: event.requestId,
        payloadJson: event.payloadJson,
      });
    } catch (error) {
      await this.failBridge(event.requestId, errorMessage(error));
    }
  }

  private async failBridge(requestId: string, error: string): Promise<void> {
    await invoke<boolean>('submit_relay_response', {
      requestId,
      payloadJson: JSON.stringify({ error }),
    });
  }
}

export const trustedWebRtc = new TrustedWebRtcRuntime();
