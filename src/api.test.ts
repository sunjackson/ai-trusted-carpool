import { afterEach, describe, expect, it, vi } from 'vitest';

const { invokeMock } = vi.hoisted(() => ({ invokeMock: vi.fn() }));

vi.mock('./tauriInvoke', () => ({ invoke: invokeMock }));

import {
  cancelAccountImport,
  cancelAccountRestore,
  checkSignedAppUpdate,
  closeClientInstance,
  commitAccountImport,
  commitAccountRestore,
  deleteAccount,
  downloadAppUpdate,
  exportAccountBackup,
  focusClientInstance,
  importAccounts,
  importLocalAccounts,
  installAppUpdate,
  launchTool,
  listRideHistory,
  listAccounts,
  listClientInstances,
  markFrontendReady,
  previewAccountImport,
  previewAccountRestore,
  retryAccountRoute,
  resumeHostCar,
  resumePassengerRide,
  restartAfterAppUpdate,
  serverJoinUrl,
  startCar,
  suspendCar,
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
  credentialState: 'normal',
  routeHealth: {
    status: 'normal',
    reason: null,
    cooldownUntilMs: null,
    consecutiveFailures: 0,
    lastAttemptAtMs: null,
    lastSuccessAtMs: null,
    lastFailureAtMs: null,
  },
};

afterEach(() => {
  invokeMock.mockReset();
  Reflect.deleteProperty(window, '__TAURI_INTERNALS__');
});

describe('desktop startup readiness', () => {
  it('signals readiness only inside the Tauri desktop runtime', async () => {
    await expect(markFrontendReady()).resolves.toBeUndefined();
    expect(invokeMock).not.toHaveBeenCalled();

    Object.defineProperty(window, '__TAURI_INTERNALS__', { configurable: true, value: {} });
    invokeMock.mockResolvedValueOnce(undefined);

    await expect(markFrontendReady()).resolves.toBeUndefined();
    expect(invokeMock).toHaveBeenCalledWith('mark_frontend_ready');
  });
});

describe('managed desktop client commands', () => {
  it('returns launch readiness and addresses instances by opaque id', async () => {
    Object.defineProperty(window, '__TAURI_INTERNALS__', { configurable: true, value: {} });
    const result = {
      instanceId: 'instance-1',
      status: 'ready',
      reused: false,
      readyAtMs: 1234,
    } as const;
    const instances = [
      {
        ...result,
        accessId: 'access-1',
        tool: 'claude',
        processId: 42,
        launchedAtMs: 1200,
      },
    ];
    invokeMock
      .mockResolvedValueOnce(result)
      .mockResolvedValueOnce(instances)
      .mockResolvedValueOnce(undefined)
      .mockResolvedValueOnce(true);

    await expect(
      launchTool({ kind: 'claude', mode: 'desktop', accessId: 'access-1' })
    ).resolves.toEqual(result);
    await expect(listClientInstances()).resolves.toEqual(instances);
    await focusClientInstance('instance-1');
    await expect(closeClientInstance('instance-1')).resolves.toBe(true);

    expect(invokeMock).toHaveBeenNthCalledWith(1, 'launch_tool', {
      input: { kind: 'claude', mode: 'desktop', accessId: 'access-1' },
    });
    expect(invokeMock).toHaveBeenNthCalledWith(2, 'list_client_instances');
    expect(invokeMock).toHaveBeenNthCalledWith(3, 'focus_client_instance', {
      instanceId: 'instance-1',
    });
    expect(invokeMock).toHaveBeenNthCalledWith(4, 'close_client_instance', {
      instanceId: 'instance-1',
    });
  });
});

describe('ride history and recovery commands', () => {
  it('forwards all-day hosting and opaque history record identifiers to Rust', async () => {
    Object.defineProperty(window, '__TAURI_INTERNALS__', { configurable: true, value: {} });
    const car = {
      carId: 'car-1',
      carName: '全天车队',
      ownerPeerId: 'owner-1',
      startedAt: 100,
      expiresAt: Number.MAX_SAFE_INTEGER,
      alwaysOn: true,
      enabledTools: ['claude'],
      seats: [],
      accountQuotas: [],
    };
    const history = [{ recordId: 'record-1', role: 'host', carId: 'car-1' }];
    const access = {
      carId: 'car-2',
      carName: '好友车队',
      ownerLabel: '好友',
      seatNo: 1,
      enabledTools: ['codex'],
      startsAt: 100,
      expiresAt: Number.MAX_SAFE_INTEGER,
      alwaysOn: true,
      accessId: 'access-1',
      ownerPeerId: 'owner-2',
      localProxyPort: 25342,
      connectionState: 'connected',
    };
    invokeMock
      .mockResolvedValueOnce(car)
      .mockResolvedValueOnce(history)
      .mockResolvedValueOnce(car)
      .mockResolvedValueOnce(access)
      .mockResolvedValueOnce(undefined);

    await expect(startCar({
      carName: '全天车队',
      enabledTools: ['claude'],
      startsAt: 100,
      endsAt: 200,
      alwaysOn: true,
    })).resolves.toEqual(car);
    await expect(listRideHistory()).resolves.toEqual(history);
    await expect(resumeHostCar('record-1')).resolves.toEqual(car);
    await expect(resumePassengerRide('record-2')).resolves.toEqual(access);
    await expect(suspendCar()).resolves.toBeUndefined();

    expect(invokeMock).toHaveBeenNthCalledWith(1, 'start_car', {
      input: {
        carName: '全天车队',
        enabledTools: ['claude'],
        startsAt: 100,
        endsAt: 200,
        alwaysOn: true,
      },
    });
    expect(invokeMock).toHaveBeenNthCalledWith(2, 'list_ride_history');
    expect(invokeMock).toHaveBeenNthCalledWith(3, 'resume_host_car', { recordId: 'record-1' });
    expect(invokeMock).toHaveBeenNthCalledWith(4, 'resume_passenger_ride', { recordId: 'record-2' });
    expect(invokeMock).toHaveBeenNthCalledWith(5, 'suspend_car');
  });
});

describe('signed application updater commands', () => {
  it('uses the Rust invoke contract and forwards Channel progress events', async () => {
    const transformCallback = vi.fn(() => 41);
    const unregisterCallback = vi.fn();
    Object.defineProperty(window, '__TAURI_INTERNALS__', {
      configurable: true,
      value: { transformCallback, unregisterCallback },
    });
    const update = {
      currentVersion: '0.0.4',
      version: '0.0.5',
      notes: 'signed release',
      date: '2026-07-22T00:00:00Z',
      installSupported: true,
      installBlockReason: null,
    } as const;
    const download = {
      update,
      downloadedBytes: 512,
      totalBytes: 1024,
    };
    const progressEvents: unknown[] = [];
    const progressChannel: {
      current: { onmessage: (event: unknown) => void; toJSON: () => string } | null;
    } = { current: null };

    invokeMock
      .mockResolvedValueOnce(update)
      .mockImplementationOnce((_command, args) => {
        progressChannel.current = args.progress;
        progressChannel.current?.onmessage({
          event: 'progress',
          downloadedBytes: 512,
          totalBytes: 1024,
        });
        return Promise.resolve(download);
      })
      .mockResolvedValueOnce(undefined)
      .mockResolvedValueOnce(undefined);

    await expect(checkSignedAppUpdate()).resolves.toEqual(update);
    await expect(downloadAppUpdate(event => progressEvents.push(event))).resolves.toEqual(download);
    await expect(installAppUpdate()).resolves.toBeUndefined();
    await expect(restartAfterAppUpdate()).resolves.toBeUndefined();

    expect(invokeMock).toHaveBeenNthCalledWith(1, 'check_signed_app_update');
    expect(invokeMock).toHaveBeenNthCalledWith(2, 'download_app_update', {
      progress: expect.any(Object),
    });
    expect(invokeMock).toHaveBeenNthCalledWith(3, 'install_app_update');
    expect(invokeMock).toHaveBeenNthCalledWith(4, 'restart_after_app_update');
    expect(progressEvents).toEqual([
      { event: 'progress', downloadedBytes: 512, totalBytes: 1024 },
    ]);
    expect(progressChannel.current?.toJSON()).toBe('__CHANNEL__:41');
    expect(transformCallback).toHaveBeenCalledTimes(1);
  });

  it('does not expose updater download or install in browser preview mode', async () => {
    await expect(downloadAppUpdate(() => undefined)).rejects.toThrow(
      '应用更新仅在桌面应用中可用'
    );
    await expect(installAppUpdate()).rejects.toThrow('应用更新仅在桌面应用中可用');
    await expect(restartAfterAppUpdate()).rejects.toThrow(
      '应用更新仅在桌面应用中可用'
    );
    expect(invokeMock).not.toHaveBeenCalled();
  });
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
      .mockResolvedValueOnce(true)
      .mockResolvedValueOnce(account);

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
    await expect(retryAccountRoute(account.id)).resolves.toEqual(account);

    expect(invokeMock).toHaveBeenNthCalledWith(1, 'list_accounts');
    expect(invokeMock).toHaveBeenNthCalledWith(2, 'import_local_accounts');
    expect(invokeMock).toHaveBeenNthCalledWith(3, 'import_accounts', {
      input: { content: 'sk-ant-secret', tool: 'claude', name: 'Claude 导入账号' },
    });
    expect(invokeMock).toHaveBeenNthCalledWith(4, 'update_account', {
      input: { id: account.id, name: 'Claude 新名称', priority: 1 },
    });
    expect(invokeMock).toHaveBeenNthCalledWith(5, 'delete_account', { id: account.id });
    expect(invokeMock).toHaveBeenNthCalledWith(6, 'retry_account_route', { id: account.id });
  });

  it('keeps preview credentials in Rust and uses explicit one-time commit contracts', async () => {
    Object.defineProperty(window, '__TAURI_INTERNALS__', { configurable: true, value: {} });
    invokeMock
      .mockResolvedValueOnce({
        session_id: 'import-session',
        expires_at_ms: 1234,
        items: [
          {
            item_id: 'item-1',
            tool: 'claude',
            auth_kind: 'oauth',
            name: 'Claude 导入账号',
            source: 'file',
            action: 'new',
          },
        ],
      })
      .mockResolvedValueOnce({ imported: 1, updated: 0, accounts: [account] })
      .mockResolvedValueOnce(true)
      .mockResolvedValueOnce('/tmp/accounts.tcarpool-backup')
      .mockResolvedValueOnce({
        sessionId: 'restore-session',
        expiresAtMs: 5678,
        mode: 'replace',
        remove_count: 1,
        items: [
          {
            itemId: 'item-2',
            tool: 'codex',
            authKind: 'apiKey',
            name: 'Codex 备份账号',
            source: 'file',
            action: 'update',
          },
        ],
      })
      .mockResolvedValueOnce({
        imported: 0,
        updated: 1,
        removed: 1,
        accounts: [account],
      })
      .mockResolvedValueOnce(true);

    await expect(
      previewAccountImport({ contents: ['{}'], source: 'file' })
    ).resolves.toEqual({
      sessionId: 'import-session',
      expiresAtMs: 1234,
      items: [
        {
          itemId: 'item-1',
          tool: 'claude',
          authKind: 'oauth',
          name: 'Claude 导入账号',
          source: 'file',
          action: 'new',
        },
      ],
    });
    await expect(commitAccountImport('import-session')).resolves.toEqual({
      imported: 1,
      updated: 0,
      accounts: [account],
    });
    await expect(cancelAccountImport('import-session')).resolves.toBe(true);
    await expect(exportAccountBackup('long-secret')).resolves.toBe(
      '/tmp/accounts.tcarpool-backup'
    );
    await expect(
      previewAccountRestore({
        content: 'encrypted-backup',
        passphrase: 'long-secret',
        mode: 'replace',
      })
    ).resolves.toMatchObject({
      sessionId: 'restore-session',
      expiresAtMs: 5678,
      mode: 'replace',
      removeCount: 1,
    });
    await expect(
      commitAccountRestore('restore-session', 'replace', true)
    ).resolves.toEqual({
      imported: 0,
      updated: 1,
      removed: 1,
      accounts: [account],
    });
    await expect(cancelAccountRestore('restore-session')).resolves.toBe(true);

    expect(invokeMock).toHaveBeenNthCalledWith(1, 'preview_account_import', {
      input: { contents: ['{}'], source: 'file' },
    });
    expect(invokeMock).toHaveBeenNthCalledWith(2, 'commit_account_import', {
      sessionId: 'import-session',
    });
    expect(invokeMock).toHaveBeenNthCalledWith(3, 'cancel_account_import', {
      sessionId: 'import-session',
    });
    expect(invokeMock).toHaveBeenNthCalledWith(4, 'export_account_backup', {
      passphrase: 'long-secret',
    });
    expect(invokeMock).toHaveBeenNthCalledWith(5, 'preview_account_restore', {
      input: {
        content: 'encrypted-backup',
        passphrase: 'long-secret',
        mode: 'replace',
      },
    });
    expect(invokeMock).toHaveBeenNthCalledWith(6, 'commit_account_restore', {
      sessionId: 'restore-session',
      mode: 'replace',
      confirmReplace: true,
    });
    expect(invokeMock).toHaveBeenNthCalledWith(7, 'cancel_account_restore', {
      sessionId: 'restore-session',
    });
  });
});
