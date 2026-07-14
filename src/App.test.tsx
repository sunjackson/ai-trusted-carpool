import '@testing-library/jest-dom/vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, describe, expect, it } from 'vitest';
import App from './App';

describe('Trusted Carpool simple flow', () => {
  afterEach(() => cleanup());

  it('keeps the welcome screen focused on two actions', () => {
    render(<App />);
    expect(screen.getByRole('button', { name: /我要发车/ })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /我要上车/ })).toBeInTheDocument();
    expect(screen.queryByText(/WebRTC|TURN|P2P|审计/)).not.toBeInTheDocument();
  });

  it('opens one-click host setup without technical controls', async () => {
    render(<App />);
    fireEvent.click(screen.getByRole('button', { name: /我要发车/ }));
    expect(await screen.findByRole('heading', { name: '选择要共享的工具' })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /开始发车/ })).toBeInTheDocument();
    expect(screen.getByText('开始时间')).toBeInTheDocument();
    expect(screen.getByText('结束时间')).toBeInTheDocument();
    expect(screen.queryByText(/押金|积分|结算/)).not.toBeInTheDocument();
    expect(screen.queryByText(/信令|中继|密钥输入/)).not.toBeInTheDocument();
  });

  it('shows live usage per person and per model with official API estimates', async () => {
    render(<App />);
    fireEvent.click(screen.getByRole('button', { name: /我要发车/ }));
    const startButton = await screen.findByRole('button', { name: /开始发车/ });
    await waitFor(() => expect(startButton).toBeEnabled());
    fireEvent.click(startButton);

    expect(await screen.findByText('claude-sonnet-4-6')).toBeInTheDocument();
    expect(screen.getByText('claude-haiku-4-5')).toBeInTheDocument();
    expect(screen.getByText('gpt-5.6-luna')).toBeInTheDocument();
    expect(screen.getAllByText('输入').length).toBeGreaterThan(0);
    expect(screen.getAllByText('输出').length).toBeGreaterThan(0);
    expect(screen.getAllByText('缓存读').length).toBeGreaterThan(0);
    expect(screen.getAllByText('缓存写').length).toBeGreaterThan(0);
    expect(screen.getByText('输入 8,400')).toBeInTheDocument();
    expect(screen.getByText('输出 2,500')).toBeInTheDocument();
    expect(screen.getByText('缓存读 5,200')).toBeInTheDocument();
    expect(screen.getByText('缓存写 1,500')).toBeInTheDocument();
    expect(screen.getByText(/缓存写入：5 分钟 1,100 · 1 小时 400/)).toBeInTheDocument();
    expect(screen.getAllByText(/官价估算 \$/).length).toBeGreaterThan(0);
    expect(screen.getByText(/明细仅保存在车主本机/)).toBeInTheDocument();
    expect(screen.getByText(/官方 API 标准价估算，不是账单/)).toBeInTheDocument();
  });

  it('opens the twelve-character join flow', () => {
    render(<App />);
    fireEvent.click(screen.getByRole('button', { name: /我要上车/ }));
    expect(screen.getByRole('heading', { name: '输入上车码' })).toBeInTheDocument();
    expect(screen.getByLabelText('输入上车码')).toHaveAttribute('value', '');
    expect(screen.getByRole('button', { name: /确认并上车/ })).toBeDisabled();
    expect(screen.queryByText(/押金|积分|结算/)).not.toBeInTheDocument();
  });
});
