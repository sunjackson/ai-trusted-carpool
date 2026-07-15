import { describe, expect, it } from 'vitest';
import { serverJoinUrl } from './api';

describe('serverJoinUrl', () => {
  it('creates an official one-click join link', () => {
    expect(serverJoinUrl('7g2k5lq8m4tz')).toBe(
      'https://p2p.cnaigc.ai/api/v1/carpool/join/7G2K5LQ8M4TZ'
    );
  });

  it('rejects malformed or ambiguous join codes', () => {
    expect(() => serverJoinUrl('../../redirect')).toThrow('上车码格式不正确');
    expect(() => serverJoinUrl('AAAAAAAAAAA1')).toThrow('上车码格式不正确');
  });
});
