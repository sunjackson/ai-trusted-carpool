export type WireMessage = {
  type:
    | 'relay_request'
    | 'relay_response'
    | 'relay_stream_event'
    | 'car_status_request'
    | 'car_status_snapshot';
  bridgeRequestId: string;
  payloadJson: string;
};

export type ChunkFrame = {
  type: 'chunk';
  messageId: string;
  index: number;
  total: number;
  data: string;
};

export type Assembly = {
  chunks: string[];
  received: number;
  expiresAt: number;
};

export const WEBRTC_CHUNK_SIZE = 12 * 1024;
export const MAX_WIRE_BASE64 = 32 * 1024 * 1024;

const bytesToBase64 = (bytes: Uint8Array): string => {
  let binary = '';
  for (let offset = 0; offset < bytes.length; offset += 32_768) {
    binary += String.fromCharCode(...bytes.subarray(offset, offset + 32_768));
  }
  return btoa(binary);
};

const base64ToString = (encoded: string): string => {
  const binary = atob(encoded);
  const bytes = Uint8Array.from(binary, character => character.charCodeAt(0));
  return new TextDecoder().decode(bytes);
};

const isWireMessage = (value: unknown): value is WireMessage => {
  if (!value || typeof value !== 'object') return false;
  const message = value as Partial<WireMessage>;
  return (
    (message.type === 'relay_request' ||
      message.type === 'relay_response' ||
      message.type === 'relay_stream_event' ||
      message.type === 'car_status_request' ||
      message.type === 'car_status_snapshot') &&
    typeof message.bridgeRequestId === 'string' &&
    message.bridgeRequestId.length > 0 &&
    typeof message.payloadJson === 'string'
  );
};

export function createChunkFrames(
  message: WireMessage,
  messageId: string = crypto.randomUUID()
): string[] {
  const encoded = bytesToBase64(new TextEncoder().encode(JSON.stringify(message)));
  if (encoded.length > MAX_WIRE_BASE64) throw new Error('中转消息超过安全大小限制');
  const total = Math.ceil(encoded.length / WEBRTC_CHUNK_SIZE) || 1;
  return Array.from({ length: total }, (_, index) =>
    JSON.stringify({
      type: 'chunk',
      messageId,
      index,
      total,
      data: encoded.slice(
        index * WEBRTC_CHUNK_SIZE,
        (index + 1) * WEBRTC_CHUNK_SIZE
      ),
    } satisfies ChunkFrame)
  );
}

export function acceptChunkFrame(
  assemblies: Map<string, Assembly>,
  raw: string,
  now = Date.now()
): WireMessage | null {
  let frame: ChunkFrame;
  try {
    frame = JSON.parse(raw) as ChunkFrame;
  } catch {
    return null;
  }
  if (
    frame.type !== 'chunk' ||
    typeof frame.messageId !== 'string' ||
    !frame.messageId ||
    !Number.isInteger(frame.total) ||
    frame.total < 1 ||
    frame.total > Math.ceil(MAX_WIRE_BASE64 / WEBRTC_CHUNK_SIZE) ||
    !Number.isInteger(frame.index) ||
    frame.index < 0 ||
    frame.index >= frame.total ||
    typeof frame.data !== 'string' ||
    frame.data.length > WEBRTC_CHUNK_SIZE
  ) {
    return null;
  }
  for (const [id, assembly] of assemblies) {
    if (assembly.expiresAt < now) assemblies.delete(id);
  }
  const assembly = assemblies.get(frame.messageId) ?? {
    chunks: Array<string>(frame.total).fill(''),
    received: 0,
    expiresAt: now + 60_000,
  };
  if (assembly.chunks.length !== frame.total) return null;
  if (!assembly.chunks[frame.index]) {
    assembly.chunks[frame.index] = frame.data;
    assembly.received += 1;
  }
  assemblies.set(frame.messageId, assembly);
  if (assembly.received !== frame.total) return null;
  assemblies.delete(frame.messageId);
  const encoded = assembly.chunks.join('');
  if (encoded.length > MAX_WIRE_BASE64) return null;
  try {
    const message: unknown = JSON.parse(base64ToString(encoded));
    return isWireMessage(message) ? message : null;
  } catch {
    return null;
  }
}
