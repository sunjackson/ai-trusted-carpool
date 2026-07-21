import { afterEach, describe, expect, it, vi } from 'vitest';

const { invokeMock } = vi.hoisted(() => ({ invokeMock: vi.fn() }));

vi.mock('./tauriInvoke', () => ({ invoke: invokeMock }));

import {
  deleteAccount,
  importAccounts,
  importLocalAccounts,
  listAccounts,
  serverJoinUrl,
  updateAccount,
} from './api';

const account = {
  id: 'claude-main',
  tool: 'claude',
  name: 'Claude 主账号',
  authKind: 'oauth',
  enabled: true,
  priority: 10,
  source: 'local',
  createdAtMs: 100,
  updatedAtMs: 100,
};

afterEach(() => {
  invokeMock.mockReset();
  Reflect.deleteProperty(window, '__TAURI_INTERNALS__');
});

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

describe('local account commands', () => {
  it('uses the Tauri account contract without forwarding legacy field names', async () => {
    Object.defineProperty(window, '__TAURI_INTERNALS__', { configurable: true, value: {} });
    invokeMock
      .mockResolvedValueOnce([account])
      .mockResolvedValueOnce({ imported: 0, updated: 1, accounts: [account] })
      .mockResolvedValueOnce({ imported: 1, updated: 0, accounts: [account] })
      .mockResolvedValueOnce(account)
      .mockResolvedValueOnce(true);

    expect(await listAccounts()).toEqual([account]);
    expect(await importLocalAccounts()).toEqual({ imported: 0, updated: 1, accounts: [account] });
    expect(
      await importAccounts({
        content: 'sk-ant-secret',
        tool: 'claude',
        displayName: 'Claude 导入账号',
      })
    ).toEqual({ imported: 1, updated: 0, accounts: [account] });
    expect(
      await updateAccount({ id: account.id, displayName: 'Claude 新名称', priority: 1 })
    ).toEqual(account);
    await deleteAccount(account.id);

    expect(invokeMock).toHaveBeenNthCalledWith(1, 'list_accounts');
    expect(invokeMock).toHaveBeenNthCalledWith(2, 'import_local_accounts');
    expect(invokeMock).toHaveBeenNthCalledWith(3, 'import_accounts', {
      input: { content: 'sk-ant-secret', tool: 'claude', name: 'Claude 导入账号' },
    });
    expect(invokeMock).toHaveBeenNthCalledWith(4, 'update_account', {
      input: { id: account.id, name: 'Claude 新名称', priority: 1 },
    });
    expect(invokeMock).toHaveBeenNthCalledWith(5, 'delete_account', { id: account.id });
  });
});
