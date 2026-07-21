import '@testing-library/jest-dom/vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { AccountImportResult, LocalAccountSummary } from './types';

const apiMocks = vi.hoisted(() => ({
  deleteAccount: vi.fn(),
  importAccounts: vi.fn(),
  importLocalAccounts: vi.fn(),
  listAccounts: vi.fn(),
  updateAccount: vi.fn(),
}));

vi.mock('./api', () => apiMocks);

import { AccountManager } from './AccountManager';

const accounts: LocalAccountSummary[] = [
  {
    id: 'claude-main',
    tool: 'claude',
    name: 'Claude 主账号',
    authKind: 'oauth',
    enabled: true,
    priority: 10,
    source: 'Claude 本机配置',
    createdAtMs: 100,
    updatedAtMs: 100,
  },
  {
    id: 'codex-backup',
    tool: 'codex',
    name: 'Codex 备用账号',
    authKind: 'apiKey',
    enabled: true,
    priority: 20,
    source: '手动导入',
    createdAtMs: 200,
    updatedAtMs: 200,
  },
];

const importResult = (
  affected: LocalAccountSummary[],
  imported = affected.length,
  updated = 0
): AccountImportResult => ({ imported, updated, accounts: affected });

describe('AccountManager', () => {
  beforeEach(() => {
    apiMocks.listAccounts.mockResolvedValue(accounts);
    apiMocks.importLocalAccounts.mockResolvedValue(importResult([accounts[0]]));
    apiMocks.importAccounts.mockResolvedValue(importResult([accounts[0]]));
    apiMocks.updateAccount.mockResolvedValue(accounts[0]);
    apiMocks.deleteAccount.mockResolvedValue(undefined);
  });

  afterEach(() => {
    cleanup();
    vi.clearAllMocks();
    vi.restoreAllMocks();
  });

  it('shows redacted summaries and maintains names, priorities, and enabled state', async () => {
    render(<AccountManager onClose={vi.fn()} />);

    expect(await screen.findByText('Claude 主账号')).toBeInTheDocument();
    expect(screen.getByText('Claude · OAuth · 凭据已隐藏')).toBeInTheDocument();
    expect(screen.getByText('Codex · API Key · 凭据已隐藏')).toBeInTheDocument();

    fireEvent.change(screen.getByLabelText('Claude 主账号 名称'), {
      target: { value: 'Claude 高优先级' },
    });
    fireEvent.change(screen.getByLabelText('Claude 主账号 优先级'), {
      target: { value: '2' },
    });
    fireEvent.click(screen.getByRole('button', { name: '保存 Claude 主账号' }));
    await waitFor(() =>
      expect(apiMocks.updateAccount).toHaveBeenCalledWith({
        id: 'claude-main',
        name: 'Claude 高优先级',
        priority: 2,
      })
    );

    fireEvent.click(screen.getByRole('checkbox', { name: '停用 Codex 备用账号' }));
    await waitFor(() =>
      expect(apiMocks.updateAccount).toHaveBeenCalledWith({ id: 'codex-backup', enabled: false })
    );
  });

  it('imports pasted credentials, local configs, and multiple JSON files', async () => {
    render(<AccountManager onClose={vi.fn()} />);
    await screen.findByText('Claude 主账号');

    fireEvent.click(screen.getByRole('button', { name: /导入本机配置/ }));
    await waitFor(() => expect(apiMocks.importLocalAccounts).toHaveBeenCalledTimes(1));
    expect(await screen.findByText('本机配置导入完成：新增 1 个账号')).toBeInTheDocument();

    fireEvent.change(screen.getByLabelText('账号类型'), { target: { value: 'claude' } });
    fireEvent.change(screen.getByLabelText('账号名称（可选）'), {
      target: { value: 'Claude 新账号' },
    });
    fireEvent.change(screen.getByLabelText('API Key 或 JSON'), {
      target: { value: 'sk-ant-secret-value' },
    });
    fireEvent.click(screen.getByRole('button', { name: '导入账号' }));
    await waitFor(() =>
      expect(apiMocks.importAccounts).toHaveBeenCalledWith({
        content: 'sk-ant-secret-value',
        tool: 'claude',
        name: 'Claude 新账号',
        source: 'json',
      })
    );
    expect(await screen.findByText('账号导入完成：新增 1 个账号')).toBeInTheDocument();
    expect(screen.queryByText('sk-ant-secret-value')).not.toBeInTheDocument();

    const first = new File(['{"OPENAI_API_KEY":"first"}'], 'first.json', { type: 'application/json' });
    const second = new File(['{"OPENAI_API_KEY":"second"}'], 'second.json', { type: 'application/json' });
    Object.defineProperty(first, 'text', { value: vi.fn().mockResolvedValue('{"OPENAI_API_KEY":"first"}') });
    Object.defineProperty(second, 'text', { value: vi.fn().mockResolvedValue('{"OPENAI_API_KEY":"second"}') });
    fireEvent.change(screen.getByLabelText('选择 JSON 文件'), {
      target: { files: [first, second] },
    });
    await waitFor(() => expect(apiMocks.importAccounts).toHaveBeenCalledTimes(3));
  });

  it('refreshes the account list and reports partial success when a later file fails', async () => {
    apiMocks.importAccounts
      .mockResolvedValueOnce(importResult([accounts[0]], 1, 0))
      .mockRejectedValueOnce(new Error('第二个文件格式无效'))
      .mockResolvedValueOnce(importResult([accounts[1]], 0, 1));
    render(<AccountManager onClose={vi.fn()} />);
    await screen.findByText('Claude 主账号');

    const first = new File(['{}'], 'first.json', { type: 'application/json' });
    const second = new File(['{}'], 'second.json', { type: 'application/json' });
    const third = new File(['{}'], 'third.json', { type: 'application/json' });
    Object.defineProperty(first, 'text', { value: vi.fn().mockResolvedValue('{}') });
    Object.defineProperty(second, 'text', { value: vi.fn().mockResolvedValue('{}') });
    Object.defineProperty(third, 'text', { value: vi.fn().mockResolvedValue('{}') });
    fireEvent.change(screen.getByLabelText('选择 JSON 文件'), {
      target: { files: [first, second, third] },
    });

    expect(
      await screen.findByText('已处理 2 个文件：新增 1 个账号，更新 1 个账号；1 个文件导入失败：second.json：第二个文件格式无效')
    ).toBeInTheDocument();
    expect(apiMocks.importAccounts).toHaveBeenCalledTimes(3);
    expect(apiMocks.listAccounts).toHaveBeenCalledTimes(2);
  });

  it('rejects oversized files before reading them and continues with later files', async () => {
    render(<AccountManager onClose={vi.fn()} />);
    await screen.findByText('Claude 主账号');

    const oversized = new File(['{}'], 'oversized.json', { type: 'application/json' });
    const oversizedText = vi.fn().mockResolvedValue('{}');
    Object.defineProperty(oversized, 'size', { value: 8 * 1024 * 1024 + 1 });
    Object.defineProperty(oversized, 'text', { value: oversizedText });
    const small = new File(['{}'], 'small.json', { type: 'application/json' });
    const smallText = vi.fn().mockResolvedValue('{}');
    Object.defineProperty(small, 'text', { value: smallText });

    fireEvent.change(screen.getByLabelText('选择 JSON 文件'), {
      target: { files: [oversized, small] },
    });

    expect(
      await screen.findByText('已处理 1 个文件：新增 1 个账号；1 个文件导入失败：oversized.json：文件过大（最大 8 MiB）')
    ).toBeInTheDocument();
    expect(oversizedText).not.toHaveBeenCalled();
    expect(smallText).toHaveBeenCalledTimes(1);
    expect(apiMocks.importAccounts).toHaveBeenCalledTimes(1);
    expect(apiMocks.importAccounts).toHaveBeenCalledWith({ content: '{}', source: 'file' });
  });

  it('accepts the backend maximum account priority', async () => {
    render(<AccountManager onClose={vi.fn()} />);
    await screen.findByText('Claude 主账号');

    const priorityInput = screen.getByLabelText('Claude 主账号 优先级');
    expect(priorityInput).toHaveAttribute('max', '1000000');
    fireEvent.change(priorityInput, { target: { value: '1000000' } });
    fireEvent.click(screen.getByRole('button', { name: '保存 Claude 主账号' }));

    await waitFor(() =>
      expect(apiMocks.updateAccount).toHaveBeenCalledWith({
        id: 'claude-main',
        name: 'Claude 主账号',
        priority: 1_000_000,
      })
    );
  });

  it('rejects a blank priority instead of treating it as highest priority', async () => {
    render(<AccountManager onClose={vi.fn()} />);
    await screen.findByText('Claude 主账号');

    fireEvent.change(screen.getByLabelText('Claude 主账号 优先级'), {
      target: { value: '' },
    });
    fireEvent.click(screen.getByRole('button', { name: '保存 Claude 主账号' }));

    expect(await screen.findByRole('alert')).toHaveTextContent(
      '优先级必须是 0—1000000 之间的整数'
    );
    expect(apiMocks.updateAccount).not.toHaveBeenCalled();
  });

  it('preserves dirty drafts while refreshing clean account rows', async () => {
    apiMocks.listAccounts
      .mockResolvedValueOnce(accounts)
      .mockResolvedValueOnce([
        accounts[0],
        { ...accounts[1], name: 'Codex 已更新', priority: 25, enabled: false },
      ]);
    render(<AccountManager onClose={vi.fn()} />);
    await screen.findByText('Claude 主账号');

    fireEvent.change(screen.getByLabelText('Claude 主账号 名称'), {
      target: { value: 'Claude 未保存' },
    });
    fireEvent.change(screen.getByLabelText('Claude 主账号 优先级'), {
      target: { value: '2' },
    });
    fireEvent.click(screen.getByRole('checkbox', { name: '停用 Codex 备用账号' }));

    await waitFor(() => expect(apiMocks.listAccounts).toHaveBeenCalledTimes(2));
    await screen.findByDisplayValue('Codex 已更新');
    expect(screen.getByDisplayValue('Claude 未保存')).toBeInTheDocument();
    expect(screen.getByDisplayValue('2')).toBeInTheDocument();
    expect(screen.getByDisplayValue('Codex 已更新')).toBeInTheDocument();
    expect(screen.getByDisplayValue('25')).toBeInTheDocument();
  });

  it('keeps maintenance actions disabled until the initial account load completes', async () => {
    let resolveAccounts: (value: LocalAccountSummary[]) => void = () => undefined;
    apiMocks.listAccounts.mockReturnValueOnce(
      new Promise<LocalAccountSummary[]>(resolve => {
        resolveAccounts = resolve;
      })
    );
    render(<AccountManager onClose={vi.fn()} />);

    const localImport = screen.getByRole('button', { name: /导入本机配置/ });
    expect(localImport).toBeDisabled();
    fireEvent.click(localImport);
    expect(apiMocks.importLocalAccounts).not.toHaveBeenCalled();

    resolveAccounts(accounts);
    await screen.findByText('Claude 主账号');
    expect(localImport).toBeEnabled();
  });

  it('deletes an account only after confirmation', async () => {
    const confirm = vi.spyOn(window, 'confirm').mockReturnValue(true);
    render(<AccountManager onClose={vi.fn()} />);
    await screen.findByText('Codex 备用账号');

    fireEvent.click(screen.getByRole('button', { name: '删除 Codex 备用账号' }));
    expect(confirm).toHaveBeenCalledWith('确认删除“Codex 备用账号”？此操作不会删除原始工具配置。');
    await waitFor(() => expect(apiMocks.deleteAccount).toHaveBeenCalledWith('codex-backup'));
  });
});
