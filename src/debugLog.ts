import { invoke as rawTauriInvoke } from '@tauri-apps/api/core';

export type DebugLogLevel = 'debug' | 'info' | 'warn' | 'error';

export type DebugLogEntry = {
  id: string;
  timestamp: number;
  level: DebugLogLevel;
  source: string;
  message: string;
};

type BackendDebugLogEntry = Omit<DebugLogEntry, 'id'> & { id: number };

const MAX_LOG_ENTRIES = 500;
const entries: DebugLogEntry[] = [];
const listeners = new Set<() => void>();
let nextId = 0;
let captureInstalled = false;

const REDACTED = '[REDACTED]';
const SENSITIVE_ASSIGNMENT_PATTERN = /((?:["'`]?)(?:access[_-]?token|refresh[_-]?token|id[_-]?token|api[_-]?key|x[_-]?api[_-]?key|openai[_-]?api[_-]?key|anthropic[_-]?api[_-]?key|authorization|client[_-]?secret|session[_-]?secret|secret|password|credential(?:s)?|cookie|token|prompt|request[_-]?body|response[_-]?body|environment)(?:["'`]?)\s*[:=]\s*)("(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|`(?:\\.|[^`\\])*`|(?:bearer|basic)\s+[^\s,;}&\]]+|[^\s,;}&\]]+)/gi;
const AUTH_SCHEME_PATTERN = /\b(bearer|basic)\s+[A-Za-z0-9._~+/=-]+/gi;
const API_KEY_PATTERN = /\bsk-(?:ant-|proj-)?[A-Za-z0-9_-]{8,}\b/gi;
const JWT_PATTERN = /\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\b/g;
const EMAIL_PATTERN = /\b[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,}\b/gi;
const JOIN_CODE_PATTERN = /\b[A-HJ-NP-Z2-9]{12}\b/g;
const USER_DIRECTORY_PATTERN = /(?:\/Users\/|\/home\/|[A-Z]:\\Users\\)[^/\\\s]+/gi;

export const redactDebugMessage = (message: string): string =>
  message
    .replace(SENSITIVE_ASSIGNMENT_PATTERN, (_match, prefix: string, value: string) => {
      const quote = value.at(0);
      return `${prefix}${quote === '"' || quote === "'" || quote === '`' ? `${quote}${REDACTED}${quote}` : REDACTED}`;
    })
    .replace(AUTH_SCHEME_PATTERN, '$1 [REDACTED]')
    .replace(API_KEY_PATTERN, REDACTED)
    .replace(JWT_PATTERN, REDACTED)
    .replace(EMAIL_PATTERN, '[EMAIL]')
    .replace(JOIN_CODE_PATTERN, '[JOIN_CODE]')
    .replace(USER_DIRECTORY_PATTERN, '~');

const formatValue = (value: unknown): string => {
  if (value instanceof Error) return value.stack || `${value.name}: ${value.message}`;
  if (typeof value === 'string') return value;
  try {
    return JSON.stringify(value, null, 2);
  } catch {
    return String(value);
  }
};

const notify = () => listeners.forEach(listener => listener());

export function debugLog(
  level: DebugLogLevel,
  source: string,
  ...values: unknown[]
): void {
  const entry: DebugLogEntry = {
    id: `frontend-${Date.now()}-${nextId++}`,
    timestamp: Date.now(),
    level,
    source,
    message: redactDebugMessage(values.map(formatValue).join(' ')),
  };
  entries.push(entry);
  if (entries.length > MAX_LOG_ENTRIES) entries.splice(0, entries.length - MAX_LOG_ENTRIES);
  if (inTauri()) {
    void rawTauriInvoke('record_frontend_log', {
      input: {
        level: entry.level,
        source: entry.source,
        message: entry.message,
        timestamp: entry.timestamp,
      },
    }).catch(() => undefined);
  }
  notify();
}

export function getDebugLogs(): DebugLogEntry[] {
  return [...entries];
}

export function subscribeToDebugLogs(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

export function clearDebugLogs(): void {
  entries.length = 0;
  notify();
}

export function installDebugCapture(): void {
  if (captureInstalled) return;
  captureInstalled = true;

  (['debug', 'info', 'warn', 'error'] as const).forEach(level => {
    const original = console[level].bind(console);
    console[level] = (...values: unknown[]) => {
      debugLog(level, 'Console', ...values);
      original(redactDebugMessage(values.map(formatValue).join(' ')));
    };
  });

  window.addEventListener('error', event => {
    debugLog('error', 'Window', event.error ?? `${event.message} (${event.filename}:${event.lineno})`);
  });
  window.addEventListener('unhandledrejection', event => {
    debugLog('error', 'Promise', event.reason);
  });

  debugLog('info', 'App', `界面启动 · ${navigator.userAgent}`);
}

const inTauri = (): boolean => '__TAURI_INTERNALS__' in window;

export async function getBackendDebugLogs(): Promise<DebugLogEntry[]> {
  if (!inTauri()) return [];
  const backendEntries = await rawTauriInvoke<BackendDebugLogEntry[]>('get_debug_logs');
  return backendEntries.map(entry => ({
    ...entry,
    id: `backend-${entry.id}`,
    source: entry.source.startsWith('frontend · ')
      ? entry.source.slice('frontend · '.length)
      : `Rust · ${entry.source}`,
    message: redactDebugMessage(entry.message),
  }));
}

export async function clearBackendDebugLogs(): Promise<void> {
  if (!inTauri()) return;
  await rawTauriInvoke('clear_debug_logs');
}

export async function openDebugLogDirectory(): Promise<string | null> {
  if (!inTauri()) return null;
  return rawTauriInvoke<string>('open_debug_log_directory');
}

export async function exportDiagnosticBundle(): Promise<string | null> {
  if (!inTauri()) return null;
  return rawTauriInvoke<string>('export_diagnostic_bundle');
}
