import '@testing-library/jest-dom/vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { ToolDetection } from './types';

const { detectToolsMock, installToolMock } = vi.hoisted(() => ({
  detectToolsMock: vi.fn(),
  installToolMock: vi.fn(),
}));

vi.mock('./api', async importOriginal => {
  const actual = await importOriginal<typeof import('./api')>();
  return { ...actual, detectTools: detectToolsMock, installTool: installToolMock };
});

import App from './App';

const missingClaude: ToolDetection = {
  kind: 'claude',
  name: 'Claude Code',
  installed: false,
  authenticated: false,
  executablePath: null,
  configPath: null,
  detail: '未安装',
  version: null,
  npmAvailable: true,
  desktopSupported: true,
  desktopInstalled: false,
  desktopPath: null,
  desktopDetail: '未找到官方客户端',
};

const readyCodex: ToolDetection = {
  kind: 'codex',
  name: 'Codex',
  installed: true,
  authenticated: true,
  executablePath: '/usr/local/bin/codex',
  configPath: '~/.codex/auth.json',
  detail: '已就绪',
  version: 'v0.140.0',
  npmAvailable: true,
  desktopSupported: true,
  desktopInstalled: false,
  desktopPath: null,
  desktopDetail: '未找到官方客户端',
};

describe('one-click CLI install', () => {
  beforeEach(() => {
    window.localStorage.setItem('trusted-carpool:risk-acknowledged', '1');
    detectToolsMock.mockResolvedValue([missingClaude, readyCodex]);
    installToolMock.mockResolvedValue({
      ...missingClaude,
      installed: true,
      executablePath: '/usr/local/bin/claude',
      detail: '缺少官方 API Key',
      version: 'v2.1.178',
    });
  });
  afterEach(() => cleanup());

  it('installs a missing CLI from the host setup page', async () => {
    render(<App />);
    fireEvent.click(screen.getByRole('button', { name: /我要发车/ }));

    const installButton = await screen.findByRole('button', { name: '一键安装 Claude' });
    expect(screen.getByText('未安装')).toBeInTheDocument();
    expect(screen.getByText('已就绪 · v0.140.0')).toBeInTheDocument();
    fireEvent.click(installButton);

    await waitFor(() => expect(installToolMock).toHaveBeenCalledWith('claude'));
    expect(await screen.findByText('缺少官方 API Key')).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: '一键安装 Claude' })).not.toBeInTheDocument();
  });

  it('disables one-click install and explains when npm is unavailable', async () => {
    detectToolsMock.mockResolvedValue([
      { ...missingClaude, npmAvailable: false, detail: '未安装，一键安装需要先装 Node.js' },
      { ...readyCodex, npmAvailable: false },
    ]);
    render(<App />);
    fireEvent.click(screen.getByRole('button', { name: /我要发车/ }));

    const installButton = await screen.findByRole('button', { name: '一键安装 Claude' });
    expect(installButton).toBeDisabled();
    expect(installButton).toHaveAttribute('title', '需要先安装 Node.js（nodejs.org）');
    expect(screen.getByText('未安装，一键安装需要先装 Node.js')).toBeInTheDocument();
  });

  it('surfaces install failures without leaving the page', async () => {
    installToolMock.mockRejectedValue(
      new Error('安装 @anthropic-ai/claude-code 失败：EACCES')
    );
    render(<App />);
    fireEvent.click(screen.getByRole('button', { name: /我要发车/ }));

    fireEvent.click(await screen.findByRole('button', { name: '一键安装 Claude' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('EACCES');
    expect(screen.getByRole('button', { name: '一键安装 Claude' })).toBeEnabled();
  });

  it('lets passengers install the missing CLI before opening the terminal', async () => {
    render(<App />);
    fireEvent.click(screen.getByRole('button', { name: /我要上车/ }));
    fireEvent.change(screen.getByLabelText('输入上车码'), {
      target: { value: '7G2K5LQ8M4TZ' },
    });
    await screen.findByText('我的高效车队');
    fireEvent.change(screen.getByPlaceholderText('例如：阿杰'), {
      target: { value: '小雨' },
    });
    fireEvent.click(screen.getByRole('button', { name: /确认并上车/ }));

    await screen.findByRole('heading', { name: '选择要打开的工具' });
    expect(screen.getByText(/还没有 Claude 命令行工具/)).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /打开 Claude 终端/ })).toBeDisabled();

    fireEvent.click(screen.getByRole('button', { name: '一键安装 Claude 命令行' }));
    await waitFor(() => expect(installToolMock).toHaveBeenCalledWith('claude'));

    expect(await screen.findByRole('button', { name: /打开 Claude 终端/ })).toBeEnabled();
    expect(screen.getByText(/已安装 v2\.1\.178/)).toBeInTheDocument();
    expect(screen.queryByText(/还没有 Claude 命令行工具/)).not.toBeInTheDocument();
  });

  it('offers install inside the ride page when the CLI is still missing', async () => {
    render(<App />);
    fireEvent.click(screen.getByRole('button', { name: /我要上车/ }));
    fireEvent.change(screen.getByLabelText('输入上车码'), {
      target: { value: '7G2K5LQ8M4TZ' },
    });
    await screen.findByText('我的高效车队');
    fireEvent.change(screen.getByPlaceholderText('例如：阿杰'), {
      target: { value: '小雨' },
    });
    fireEvent.click(screen.getByRole('button', { name: /确认并上车/ }));
    await screen.findByRole('heading', { name: '选择要打开的工具' });

    // Open Codex (installed) first, then install Claude from the ride page.
    fireEvent.click(screen.getByRole('button', { name: /Codex/ }));
    fireEvent.click(screen.getByRole('button', { name: /打开 Codex 终端/ }));
    await screen.findByRole('heading', { name: '需要哪个，点哪个' });

    fireEvent.click(screen.getByRole('button', { name: '安装命令行' }));
    await waitFor(() => expect(installToolMock).toHaveBeenCalledWith('claude'));
    expect(await screen.findByRole('button', { name: /^终端/ })).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: '安装命令行' })).not.toBeInTheDocument();
  });
});
