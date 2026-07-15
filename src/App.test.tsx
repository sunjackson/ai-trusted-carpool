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
    expect(screen.getByText('什么时候开始')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: '立即开始' })).toHaveAttribute('aria-pressed', 'true');
    expect(screen.getByText('共享多久')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /2 小时/ })).toHaveAttribute('aria-pressed', 'true');
    expect(screen.queryByLabelText('预约开始时间')).not.toBeInTheDocument();
    expect(screen.queryByLabelText('自定义结束时间')).not.toBeInTheDocument();
    expect(screen.queryByText(/押金|积分|结算/)).not.toBeInTheDocument();
    expect(screen.queryByText(/信令|中继|密钥输入/)).not.toBeInTheDocument();
  });

  it('keeps common schedules one click away and reveals exact times only when requested', async () => {
    render(<App />);
    fireEvent.click(screen.getByRole('button', { name: /我要发车/ }));
    await screen.findByRole('heading', { name: '选择要共享的工具' });

    fireEvent.click(screen.getByRole('button', { name: '4 小时' }));
    expect(screen.getByRole('button', { name: '4 小时' })).toHaveAttribute('aria-pressed', 'true');
    expect(screen.getByText(/共 4 小时/)).toBeInTheDocument();

    fireEvent.click(screen.getByRole('button', { name: '预约开始' }));
    expect(screen.getByLabelText('预约开始时间')).toBeInTheDocument();

    fireEvent.click(screen.getByRole('button', { name: '自定义' }));
    expect(screen.getByLabelText('自定义结束时间')).toBeInTheDocument();
    expect(screen.queryByText(/共 4 小时/)).not.toBeInTheDocument();
  });

  it('keeps the member list concise and opens detailed usage on demand', async () => {
    render(<App />);
    fireEvent.click(screen.getByRole('button', { name: /我要发车/ }));
    const startButton = await screen.findByRole('button', { name: /开始发车/ });
    await waitFor(() => expect(startButton).toBeEnabled());
    fireEvent.click(startButton);

    expect(await screen.findByText('车队账号额度')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /查看阿杰详情/ })).toBeInTheDocument();
    expect(screen.queryByText('claude-sonnet-4-6')).not.toBeInTheDocument();
    expect(screen.queryByText('gpt-5.6-luna')).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: /查看阿杰详情/ }));

    expect(await screen.findByText('claude-sonnet-4-6')).toBeInTheDocument();
    expect(screen.getByText('claude-haiku-4-5')).toBeInTheDocument();
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
    expect(screen.getByLabelText('5 小时 Token 限额')).toHaveValue(60000);
    expect(screen.getByLabelText('24 小时 Token 限额')).toHaveValue(180000);
    expect(screen.getByLabelText('7 天 Token 限额')).toHaveValue(800000);
    const saveButton = screen.getByRole('button', { name: '保存成员限额' });
    expect(saveButton).toBeEnabled();
    fireEvent.change(screen.getByLabelText('5 小时 Token 限额'), { target: { value: '0' } });
    expect(screen.getByRole('alert')).toHaveTextContent('限额必须是 1—1万亿之间的整数');
    expect(saveButton).toBeDisabled();
    fireEvent.change(screen.getByLabelText('5 小时 Token 限额'), { target: { value: '' } });
    expect(saveButton).toBeEnabled();
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

  it('lets passengers choose desktop clients or terminals after joining', async () => {
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

    expect(await screen.findByRole('heading', { name: '选择要打开的工具' })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /^客户端/ })).toHaveAttribute('aria-pressed', 'true');
    expect(screen.getByRole('button', { name: /打开 Claude 客户端/ })).toBeEnabled();
    expect(screen.queryByText('项目目录（可选）')).not.toBeInTheDocument();

    fireEvent.click(screen.getByRole('button', { name: /^终端/ }));
    expect(screen.getByText('项目目录（可选）')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /打开 Claude 终端/ })).toBeEnabled();

    fireEvent.click(screen.getByRole('button', { name: /^客户端/ }));
    fireEvent.click(screen.getByRole('button', { name: /打开 Claude 客户端/ }));
    expect(await screen.findByRole('heading', { name: '需要哪个，点哪个' })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /新客户端/ })).toBeInTheDocument();
    expect(screen.getAllByRole('button', { name: /终端/ })).toHaveLength(2);
  });
});
