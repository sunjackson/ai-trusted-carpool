import { afterEach, describe, expect, it, vi } from 'vitest';

const { invokeMock } = vi.hoisted(() => ({ invokeMock: vi.fn() }));

vi.mock('@tauri-apps/api/core', () => ({ invoke: invokeMock }));

import {
  clearDebugLogs,
  debugLog,
  getBackendDebugLogs,
  getDebugLogs,
  redactDebugMessage,
} from './debugLog';

afterEach(() => {
  clearDebugLogs();
  invokeMock.mockReset();
  Reflect.deleteProperty(window, '__TAURI_INTERNALS__');
});

describe('debug log redaction', () => {
  it('redacts structured secrets, bearer tokens, API keys, and JWTs before storage', () => {
    const accessToken = 'access-token-secret';
    const refreshToken = 'refresh-token-secret';
    const apiKey = 'sk-ant-secret-value-123456';
    const bearer = 'bearer.secret-value';
    const jwt = 'eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.c2lnbmF0dXJlMTIzNA';

    debugLog(
      'error',
      'Test',
      {
        access_token: accessToken,
        refreshToken,
        nested: { OPENAI_API_KEY: apiKey, password: 'password-secret', safe: 'visible' },
      },
      `Authorization: Bearer ${bearer}`,
      `standalone ${apiKey}`,
      jwt
    );

    const [entry] = getDebugLogs();
    expect(entry.message).toContain('visible');
    expect(entry.message).toContain('[REDACTED]');
    for (const secret of [accessToken, refreshToken, apiKey, bearer, jwt, 'password-secret']) {
      expect(entry.message).not.toContain(secret);
    }
  });

  it('redacts credential assignments in text without hiding ordinary diagnostics', () => {
    const message = redactDebugMessage(
      'request failed apiKey=plain-api-secret; account=primary; token: "token-secret"'
    );

    expect(message).toContain('request failed');
    expect(message).toContain('account=primary');
    expect(message).not.toContain('plain-api-secret');
    expect(message).not.toContain('token-secret');
  });

  it('redacts identity, join codes, user directories, and request payloads', () => {
    const message = redactDebugMessage(
      'email=user@example.com code=ABCD2345EFGH prompt="private prompt" path=/Users/alice/project response_body=private-response'
    );

    expect(message).toContain('[EMAIL]');
    expect(message).toContain('[JOIN_CODE]');
    expect(message).toContain('~/project');
    for (const secret of ['user@example.com', 'ABCD2345EFGH', 'private prompt', 'private-response', '/Users/alice']) {
      expect(message).not.toContain(secret);
    }
  });

  it('forwards only the redacted frontend entry to persistent Rust logging', () => {
    Object.defineProperty(window, '__TAURI_INTERNALS__', { configurable: true, value: {} });
    invokeMock.mockResolvedValue(undefined);

    debugLog('error', 'UI', 'Authorization: Bearer frontend-secret user@example.com');

    expect(invokeMock).toHaveBeenCalledWith('record_frontend_log', {
      input: expect.objectContaining({
        level: 'error',
        source: 'UI',
        message: expect.stringContaining('[REDACTED]'),
      }),
    });
    const input = invokeMock.mock.calls[0][1].input;
    expect(input.message).not.toContain('frontend-secret');
    expect(input.message).not.toContain('user@example.com');
  });

  it('redacts backend messages before exposing them to the debug panel', async () => {
    Object.defineProperty(window, '__TAURI_INTERNALS__', { configurable: true, value: {} });
    invokeMock.mockResolvedValue([
      {
        id: 7,
        timestamp: 100,
        level: 'error',
        source: 'relay',
        message: 'upstream rejected Authorization: Bearer backend-secret',
      },
    ]);

    const [entry] = await getBackendDebugLogs();

    expect(entry.source).toBe('Rust · relay');
    expect(entry.message).toContain('[REDACTED]');
    expect(entry.message).not.toContain('backend-secret');
  });
});
