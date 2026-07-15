import { describe, expect, it } from 'vitest';
import {
  acceptChunkFrame,
  createChunkFrames,
  type Assembly,
  type WireMessage,
} from './webrtcWire';

describe('trusted WebRTC wire protocol', () => {
  it('round-trips a multi-megabyte relay payload through bounded chunks', () => {
    const message: WireMessage = {
      type: 'relay_request',
      bridgeRequestId: 'bridge-1',
      payloadJson: JSON.stringify({ body: '可信拼车'.repeat(220_000) }),
    };
    const frames = createChunkFrames(message, 'message-1');
    expect(frames.length).toBeGreaterThan(100);
    const assemblies = new Map<string, Assembly>();
    let decoded: WireMessage | null = null;
    for (const frame of frames) decoded = acceptChunkFrame(assemblies, frame);
    expect(decoded).toEqual(message);
    expect(assemblies.size).toBe(0);
  });

  it('ignores malformed frames and safely reconstructs a retransmitted message', () => {
    const assemblies = new Map<string, Assembly>();
    expect(acceptChunkFrame(assemblies, '{not-json')).toBeNull();
    expect(
      acceptChunkFrame(
        assemblies,
        JSON.stringify({ type: 'chunk', messageId: 'x', index: 2, total: 1, data: '' })
      )
    ).toBeNull();
    const [single] = createChunkFrames(
      { type: 'relay_response', bridgeRequestId: 'bridge-2', payloadJson: '{}' },
      'message-2'
    );
    expect(acceptChunkFrame(assemblies, single)).toEqual({
      type: 'relay_response',
      bridgeRequestId: 'bridge-2',
      payloadJson: '{}',
    });
    expect(acceptChunkFrame(assemblies, single)).toEqual({
      type: 'relay_response',
      bridgeRequestId: 'bridge-2',
      payloadJson: '{}',
    });
  });

  it('preserves ordered stream events including large base64 chunks', () => {
    const events: WireMessage[] = [
      {
        type: 'relay_stream_event',
        bridgeRequestId: 'stream-1',
        payloadJson: JSON.stringify({
          requestId: 'stream-1',
          kind: 'start',
          statusCode: 200,
          headers: [{ name: 'content-type', value: 'text/event-stream' }],
        }),
      },
      {
        type: 'relay_stream_event',
        bridgeRequestId: 'stream-1',
        payloadJson: JSON.stringify({
          requestId: 'stream-1',
          kind: 'chunk',
          chunkBase64: 'YQ=='.repeat(20_000),
        }),
      },
      {
        type: 'relay_stream_event',
        bridgeRequestId: 'stream-1',
        payloadJson: JSON.stringify({
          requestId: 'stream-1',
          kind: 'end',
          bodySha256: 'sha256:test',
        }),
      },
    ];
    const decoded = events.map((event, index) => {
      const assemblies = new Map<string, Assembly>();
      let result: WireMessage | null = null;
      for (const frame of createChunkFrames(event, `stream-frame-${index}`)) {
        result = acceptChunkFrame(assemblies, frame);
      }
      return result;
    });
    expect(decoded).toEqual(events);
  });

  it('carries member-safe car quota snapshots over the same ordered channel', () => {
    const message: WireMessage = {
      type: 'car_status_snapshot',
      bridgeRequestId: 'status-1',
      payloadJson: JSON.stringify({
        carId: 'car-1',
        accountQuotas: [{ tool: 'codex', windows: [{ label: '5 小时', remainingPercent: 58 }] }],
        member: { seatNo: 2, nickname: '小雨' },
      }),
    };
    const assemblies = new Map<string, Assembly>();
    let decoded: WireMessage | null = null;
    for (const frame of createChunkFrames(message, 'status-frame')) {
      decoded = acceptChunkFrame(assemblies, frame);
    }
    expect(decoded).toEqual(message);
  });
});
