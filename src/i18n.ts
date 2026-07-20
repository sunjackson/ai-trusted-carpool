export type Locale = 'zh-CN' | 'en';

const LOCALE_KEY = 'trusted-carpool:locale';

// 最小 i18n 骨架：默认 zh-CN，通过 localStorage 的 trusted-carpool:locale
// 切换为 en。目前只迁移了首启确认页与欢迎页作为示范，其余页面的迁移
// 登记在 CONTRIBUTING.md 的 good first issues 中。
const zhCN = {
  'brand.tagline': '共享算力，不共享密钥',
  'titlebar.officialOnly': '只连官方地址',
  'common.back': '返回',
  'firstRun.eyebrow': '使用前请先了解',
  'firstRun.title': '共享账号有被封禁的风险',
  'firstRun.pointKeys': '密钥只保存在车主电脑，乘客的请求由车主本机转发到官方地址。',
  'firstRun.pointTerms':
    'Anthropic 与 OpenAI 的官方条款不允许把账号提供给他人使用；共享账号可能被限流或封禁，且通常不予退款。',
  'firstRun.pointTrust': '请只与彼此信任的熟人拼车，禁止出租、出售座位；风险由账号所有者自行承担。',
  'firstRun.confirm': '我已知悉风险，继续',
  'firstRun.note': '完整说明见项目主页的使用须知（LEGAL.md），此确认只出现一次。',
  'welcome.eyebrow': 'TRUSTED CARPOOL',
  'welcome.title': '一起用，密钥仍只在你的电脑里',
  'welcome.lead': '发车的人保持应用在线，上车的人点一次就能打开工具。',
  'welcome.host': '我要发车',
  'welcome.hostHint': '选本机账号，分享四个上车码',
  'welcome.join': '我要上车',
  'welcome.joinHint': '输入上车码，立即打开工具',
  'welcome.trustLocal': '本机数据',
  'welcome.trustAuto': '自动连接',
  'welcome.trustLeave': '随时退出',
} as const;

export type MessageKey = keyof typeof zhCN;

const en: Record<MessageKey, string> = {
  'brand.tagline': 'Share compute, never keys',
  'titlebar.officialOnly': 'Official endpoints only',
  'common.back': 'Back',
  'firstRun.eyebrow': 'Before you start',
  'firstRun.title': 'Sharing an account can get it banned',
  'firstRun.pointKeys':
    'Credentials stay on the host machine; passenger requests are relayed to official endpoints by the host.',
  'firstRun.pointTerms':
    'Anthropic and OpenAI terms prohibit making your account available to others; shared accounts may be rate-limited or banned, usually without a refund.',
  'firstRun.pointTrust':
    'Carpool only with people you personally trust. Renting or selling seats is forbidden; the account owner bears all risk.',
  'firstRun.confirm': 'I understand the risk, continue',
  'firstRun.note': 'Full notice: LEGAL.md on the project page. This confirmation appears only once.',
  'welcome.eyebrow': 'TRUSTED CARPOOL',
  'welcome.title': 'Use it together — keys stay on your machine',
  'welcome.lead': 'The host keeps the app online; passengers open a tool with one click.',
  'welcome.host': 'Start a car',
  'welcome.hostHint': 'Pick a local account, share four seat codes',
  'welcome.join': 'Join a car',
  'welcome.joinHint': 'Enter a seat code, open a tool right away',
  'welcome.trustLocal': 'Local-only data',
  'welcome.trustAuto': 'Auto connect',
  'welcome.trustLeave': 'Leave anytime',
};

const dictionaries: Record<Locale, Record<MessageKey, string>> = {
  'zh-CN': zhCN,
  en,
};

export function getLocale(): Locale {
  try {
    const stored = window.localStorage.getItem(LOCALE_KEY);
    if (stored === 'zh-CN' || stored === 'en') return stored;
  } catch {
    // Fall through to the default locale when storage is blocked.
  }
  return 'zh-CN';
}

export function setLocale(locale: Locale): void {
  try {
    window.localStorage.setItem(LOCALE_KEY, locale);
  } catch {
    // A blocked storage API only means the preference is not persisted.
  }
}

export function t(key: MessageKey): string {
  return dictionaries[getLocale()][key] ?? zhCN[key];
}
