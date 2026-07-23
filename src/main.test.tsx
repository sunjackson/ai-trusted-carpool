import '@testing-library/jest-dom/vitest';
import { cleanup, screen, waitFor } from '@testing-library/react';
import { afterEach, expect, it, vi } from 'vitest';

const { markFrontendReadyMock } = vi.hoisted(() => ({
  markFrontendReadyMock: vi.fn(() => Promise.resolve()),
}));

vi.mock('./api', () => ({
  markFrontendReady: markFrontendReadyMock,
}));
vi.mock('./App', () => ({
  default: () => <main>可信拼车界面已就绪</main>,
}));
vi.mock('./debugLog', () => ({
  debugLog: vi.fn(),
  installDebugCapture: vi.fn(),
}));

afterEach(() => {
  cleanup();
  document.body.innerHTML = '';
  markFrontendReadyMock.mockClear();
  vi.resetModules();
});

it('replaces the packaged boot surface and signals native readiness after React commits', async () => {
  document.body.innerHTML = `
    <div id="root">
      <main id="boot-splash">正在安全启动</main>
    </div>
  `;

  await import('./main');

  expect(await screen.findByText('可信拼车界面已就绪')).toBeInTheDocument();
  expect(document.getElementById('boot-splash')).toBeNull();
  await waitFor(() => expect(markFrontendReadyMock).toHaveBeenCalled());
});
