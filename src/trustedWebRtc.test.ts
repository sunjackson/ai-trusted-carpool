import { describe, expect, it, vi } from 'vitest';
import type { IceServer } from './types';
import {
  addOfficialTurnFallback,
  trustedWebRtc,
  waitForIceGatheringComplete,
} from './trustedWebRtc';

class FakePeerConnection extends EventTarget {
  iceGatheringState: RTCIceGatheringState = 'gathering';
}

describe('Windows-compatible WebRTC setup', () => {
  it('adds the pinned official TURN address without trusting lookalike hosts', () => {
    const servers: IceServer[] = [
      {
        urls: [
          'turn:p2p.cnaigc.ai:3478?transport=udp',
          'turn:p2p.cnaigc.ai:3478?transport=tcp',
          'turn:p2p.cnaigc.ai.evil.example:3478?transport=tcp',
          'turn:relay.example.org:3478?transport=udp',
        ],
        username: 'temporary-user',
        credential: 'temporary-credential',
      },
    ];

    expect(addOfficialTurnFallback(servers)[0].urls).toEqual([
      'turn:p2p.cnaigc.ai:3478?transport=udp',
      'turn:192.220.24.20:3478?transport=udp',
      'turn:p2p.cnaigc.ai:3478?transport=tcp',
      'turn:192.220.24.20:3478?transport=tcp',
      'turn:p2p.cnaigc.ai.evil.example:3478?transport=tcp',
      'turn:relay.example.org:3478?transport=udp',
    ]);
  });

  it('waits for bundled ICE candidates before publishing an SDP description', async () => {
    const connection = new FakePeerConnection();
    const waiting = waitForIceGatheringComplete(
      connection as unknown as RTCPeerConnection,
      1_000
    );

    connection.iceGatheringState = 'complete';
    connection.dispatchEvent(new Event('icegatheringstatechange'));

    await expect(waiting).resolves.toBe(true);
  });

  it('continues with trickle ICE when gathering does not finish in time', async () => {
    const connection = new FakePeerConnection();
    await expect(
      waitForIceGatheringComplete(connection as unknown as RTCPeerConnection, 1)
    ).resolves.toBe(false);
  });

  it('does not lose completion between the initial check and listener setup', async () => {
    let reads = 0;
    const connection = new FakePeerConnection();
    Object.defineProperty(connection, 'iceGatheringState', {
      get: () => (++reads === 1 ? 'gathering' : 'complete'),
    });

    await expect(
      waitForIceGatheringComplete(connection as unknown as RTCPeerConnection, 1_000)
    ).resolves.toBe(true);
  });

  it('keeps applying later candidates when one candidate is incompatible', async () => {
    const addIceCandidate = vi
      .fn()
      .mockRejectedValueOnce(new Error('unsupported candidate'))
      .mockResolvedValueOnce(undefined);
    const connection = {
      pc: { addIceCandidate },
      pendingCandidates: [
        { candidate: 'candidate:first' },
        { candidate: 'candidate:second' },
      ],
    };
    const runtime = trustedWebRtc as unknown as {
      flushCandidates(value: typeof connection): Promise<void>;
    };

    await expect(runtime.flushCandidates(connection)).resolves.toBeUndefined();
    expect(addIceCandidate).toHaveBeenCalledTimes(2);
    expect(connection.pendingCandidates).toEqual([]);
  });
});
