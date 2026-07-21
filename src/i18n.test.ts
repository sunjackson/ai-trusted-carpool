import { afterEach, describe, expect, it } from 'vitest';
import { getLocale, setLocale, t } from './i18n';

describe('i18n skeleton', () => {
  afterEach(() => window.localStorage.removeItem('trusted-carpool:locale'));

  it('defaults to Simplified Chinese', () => {
    expect(getLocale()).toBe('zh-CN');
    expect(t('welcome.host')).toBe('我要发车');
  });

  it('switches to English through the stored preference', () => {
    setLocale('en');
    expect(getLocale()).toBe('en');
    expect(t('welcome.host')).toBe('Start a car');
    expect(t('firstRun.confirm')).toBe('I understand the risk, continue');
  });

  it('ignores unknown stored locales', () => {
    window.localStorage.setItem('trusted-carpool:locale', 'fr');
    expect(getLocale()).toBe('zh-CN');
  });
});
