import '@testing-library/jest-dom/vitest';
import { act, cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { ToolDetection, ToolInstallProgress } from './types';

const {
  detectToolsMock,
  installToolMock,
  cancelToolInstallMock,
  checkAppUpdateMock,
  openReleasesPageMock,
  progressListeners,
} = vi.hoisted(() => ({
  detectToolsMock: vi.fn(),
  installToolMock: vi.fn(),
  cancelToolInstallMock: vi.fn(),
  checkAppUpdateMock: vi.fn(),
  openReleasesPageMock: vi.fn(),
  progressListeners: [] as ((progress: ToolInstallProgress) => void)[],
}));

vi.mock('./api', async importOriginal => {
  const actual = await importOriginal<typeof import('./api')>();
  return {
    ...actual,
    detectTools: detectToolsMock,
    installTool: installToolMock,
    cancelToolInstall: cancelToolInstallMock,
    checkAppUpdate: checkAppUpdateMock,
    openReleasesPage: openReleasesPageMock,
    listenForToolInstallProgress: (onProgress: (progress: ToolInstallProgress) => void) => {
      progressListeners.push(onProgress);
      return Promise.resolve(() => {});
    },
  };
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
  npmAvailable: false,
  managedByApp: false,
  latestVersion: null,
  updateAvailable: false,
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
  npmAvailable: false,
  managedByApp: false,
  latestVersion: null,
  updateAvailable: false,
  desktopSupported: true,
  desktopInstalled: false,
  desktopPath: null,
  desktopDetail: '未找到官方客户端',
};

const installedClaude: ToolDetection = {
  ...missingClaude,
  installed: true,
  executablePath: '/managed/tools/claude/2.1.205/claude',
  detail: '缺少官方 API Key',
  version: 'v2.1.205',
  managedByApp: true,
};

const emitProgress = (progress: ToolInstallProgress) => {
  act(() => progressListeners.forEach(listener => listener(progress)));
};

describe('zero-dependency one-click provisioning', () => {
  beforeEach(() => {
    window.localStorage.setItem('trusted-carpool:risk-acknowledged', '1');
    progressListeners.length = 0;
    detectToolsMock.mockResolvedValue([missingClaude, readyCodex]);
    installToolMock.mockResolvedValue(installedClaude);
    cancelToolInstallMock.mockResolvedValue(true);
    checkAppUpdateMock.mockResolvedValue(null);
    openReleasesPageMock.mockResolvedValue(undefined);
  });
  afterEach(() => cleanup());

  it('installs the official native binary without Node.js from the host page', async () => {
    render(<App />);
    fireEvent.click(screen.getByRole('button', { name: /我要发车/ }));

    const installButton = await screen.findByRole('button', { name: '一键安装 Claude' });
    expect(installButton).toBeEnabled();
    expect(screen.getByText('未安装')).toBeInTheDocument();
    expect(screen.getByText('已就绪 · v0.140.0')).toBeInTheDocument();
    fireEvent.click(installButton);

    await waitFor(() => expect(installToolMock).toHaveBeenCalledWith('claude'));
    expect(await screen.findByText('缺少官方 API Key')).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: '一键安装 Claude' })).not.toBeInTheDocument();
  });

  it('streams official download progress and allows cancelling', async () => {
    let finishInstall: (value: ToolDetection) => void = () => undefined;
    installToolMock.mockImplementation(
      () => new Promise<ToolDetection>(resolve => (finishInstall = resolve))
    );
    render(<App />);
    fireEvent.click(screen.getByRole('button', { name: /我要发车/ }));
    fireEvent.click(await screen.findByRole('button', { name: '一键安装 Claude' }));

    emitProgress({ kind: 'claude', phase: 'resolving', receivedBytes: 0, totalBytes: null, version: null });
    expect(await screen.findByText(/获取官方版本/)).toBeInTheDocument();

    emitProgress({
      kind: 'claude',
      phase: 'downloading',
      receivedBytes: 96_000_000,
      totalBytes: 240_000_000,
      version: '2.1.205',
    });
    expect(await screen.findByText('下载中 96 MB / 240 MB · 40%')).toBeInTheDocument();

    fireEvent.click(screen.getByRole('button', { name: '取消安装 Claude' }));
    expect(cancelToolInstallMock).toHaveBeenCalledWith('claude');

    act(() => finishInstall(installedClaude));
    expect(await screen.findByText('缺少官方 API Key')).toBeInTheDocument();
  });

  it('surfaces install failures without leaving the page', async () => {
    installToolMock.mockRejectedValue(new Error('下载文件校验失败（内容与官方发布不一致），已删除。请重试'));
    render(<App />);
    fireEvent.click(screen.getByRole('button', { name: /我要发车/ }));

    fireEvent.click(await screen.findByRole('button', { name: '一键安装 Claude' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('校验失败');
    expect(screen.getByRole('button', { name: '一键安装 Claude' })).toBeEnabled();
  });

  it('downloads and opens a missing CLI with a single passenger action', async () => {
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
    expect(screen.getByText(/首次使用会从官方渠道自动下载/)).toBeInTheDocument();

    fireEvent.click(screen.getByRole('button', { name: /下载并打开 Claude 终端/ }));
    await waitFor(() => expect(installToolMock).toHaveBeenCalledWith('claude'));

    expect(await screen.findByRole('heading', { name: '需要哪个，点哪个' })).toBeInTheDocument();
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

  it('announces newer app releases and opens the pinned official page', async () => {
    checkAppUpdateMock.mockResolvedValue({
      currentVersion: '0.1.0',
      latestVersion: 'v0.2.0',
      releaseUrl: 'https://github.com/sunjackson/ai-trusted-carpool/releases',
    });
    render(<App />);
    const pill = await screen.findByRole('button', { name: /新版本 v0\.2\.0/ });
    fireEvent.click(pill);
    fireEvent.click(await screen.findByRole('button', { name: '打开官方发布页' }));
    expect(openReleasesPageMock).toHaveBeenCalled();
  });

  it('stays quiet when the app is already current', async () => {
    render(<App />);
    await screen.findByRole('button', { name: /我要发车/ });
    expect(screen.queryByRole('button', { name: /新版本/ })).not.toBeInTheDocument();
  });

  it('offers a one-click update for app-managed CLIs', async () => {
    detectToolsMock.mockResolvedValue([
      {
        ...installedClaude,
        authenticated: true,
        detail: '已就绪',
        version: 'v2.1.200',
        latestVersion: 'v2.1.205',
        updateAvailable: true,
      },
      readyCodex,
    ]);
    installToolMock.mockResolvedValue({
      ...installedClaude,
      authenticated: true,
      detail: '已就绪',
      version: 'v2.1.205',
      latestVersion: 'v2.1.205',
      updateAvailable: false,
    });
    render(<App />);
    fireEvent.click(screen.getByRole('button', { name: /我要发车/ }));

    expect(await screen.findByText('已就绪 · v2.1.200 · 应用托管')).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: '更新到 v2.1.205' }));
    await waitFor(() => expect(installToolMock).toHaveBeenCalledWith('claude'));
    expect(await screen.findByText('已就绪 · v2.1.205 · 应用托管')).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: /更新到/ })).not.toBeInTheDocument();
  });
});
