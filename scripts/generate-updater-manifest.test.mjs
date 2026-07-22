import assert from 'node:assert/strict';
import { mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import test from 'node:test';

import { buildUpdaterManifest } from './generate-updater-manifest.mjs';

async function fixture(files) {
  const directory = await mkdtemp(join(tmpdir(), 'trusted-carpool-updater-'));
  await Promise.all(
    Object.entries(files).map(async ([name, content]) => {
      const path = join(directory, name);
      await mkdir(dirname(path), { recursive: true });
      await writeFile(path, content);
    }),
  );
  return directory;
}

const input = assetsDirectory => ({
  assetsDirectory,
  version: '0.0.5',
  tag: 'v0.0.5',
  repository: 'sunjackson/ai-trusted-carpool',
  notes: '中文更新说明\n\nEnglish release notes.',
  publicationDate: '2026-07-22T00:00:00.000Z',
});

test('discovers the nested directories preserved by merged Actions artifacts', async t => {
  const directory = await fixture({
    'nsis/可信拼车_0.0.5_x64-setup.exe': 'installer',
    'nsis/可信拼车_0.0.5_x64-setup.exe.sig': 'windows-signature',
    'appimage/可信拼车_0.0.5_amd64.AppImage': 'appimage',
    'appimage/可信拼车_0.0.5_amd64.AppImage.sig': 'linux-signature',
    'macos/可信拼车.app.tar.gz': 'manual-mac',
    'macos/可信拼车.app.tar.gz.sig': 'manual-mac-signature',
  });
  t.after(() => rm(directory, { recursive: true, force: true }));

  const manifest = await buildUpdaterManifest(input(directory));

  assert.match(manifest.platforms['windows-x86_64-nsis'].url, /x64-setup\.exe$/);
  assert.match(manifest.platforms['linux-x86_64-appimage'].url, /amd64\.AppImage$/);
});

test('emits only the signed Windows and AppImage updater targets', async t => {
  const directory = await fixture({
    '可信拼车_0.0.5_x64-setup.exe': 'installer',
    '可信拼车_0.0.5_x64-setup.exe.sig': 'windows-signature',
    '可信拼车_0.0.5_amd64.AppImage': 'appimage',
    '可信拼车_0.0.5_amd64.AppImage.sig': 'linux-signature',
    '可信拼车.app.tar.gz': 'mac-update-disabled',
    '可信拼车.app.tar.gz.sig': 'mac-signature',
    '可信拼车_0.0.5_amd64.deb': 'manual-deb',
  });
  t.after(() => rm(directory, { recursive: true, force: true }));

  const manifest = await buildUpdaterManifest(input(directory));

  assert.deepEqual(Object.keys(manifest.platforms), [
    'windows-x86_64-nsis',
    'linux-x86_64-appimage',
  ]);
  assert.equal(manifest.platforms['windows-x86_64-nsis'].signature, 'windows-signature');
  assert.match(
    manifest.platforms['windows-x86_64-nsis'].url,
    /%E5%8F%AF%E4%BF%A1%E6%8B%BC%E8%BD%A6_0\.0\.5_x64-setup\.exe$/,
  );
  assert.equal(manifest.platforms['linux-x86_64-appimage'].signature, 'linux-signature');
  assert.equal(manifest.notes, '中文更新说明\n\nEnglish release notes.');
});

test('fails closed when an updater signature is missing', async t => {
  const directory = await fixture({
    'app_0.0.5_x64-setup.exe': 'installer',
    'app_0.0.5_amd64.AppImage': 'appimage',
    'app_0.0.5_amd64.AppImage.sig': 'linux-signature',
  });
  t.after(() => rm(directory, { recursive: true, force: true }));

  await assert.rejects(buildUpdaterManifest(input(directory)), /missing its Tauri signature/);
});

test('fails closed when more than one artifact matches a platform', async t => {
  const directory = await fixture({
    'app_0.0.5_x64-setup.exe': 'installer',
    'app_0.0.5_x64-setup.exe.sig': 'signature-a',
    'other_0.0.5_x64-setup.exe': 'installer',
    'other_0.0.5_x64-setup.exe.sig': 'signature-b',
    'app_0.0.5_amd64.AppImage': 'appimage',
    'app_0.0.5_amd64.AppImage.sig': 'linux-signature',
  });
  t.after(() => rm(directory, { recursive: true, force: true }));

  await assert.rejects(buildUpdaterManifest(input(directory)), /exactly one artifact, found 2/);
});

test('rejects version and tag mismatches before publishing URLs', async t => {
  const directory = await fixture({});
  t.after(() => rm(directory, { recursive: true, force: true }));

  await assert.rejects(
    buildUpdaterManifest({ ...input(directory), version: 'latest' }),
    /Invalid semantic version/,
  );
  await assert.rejects(
    buildUpdaterManifest({
      ...input(directory),
      version: '0.0.5-01',
      tag: 'v0.0.5-01',
    }),
    /Invalid semantic version/,
  );
  await assert.rejects(
    buildUpdaterManifest({ ...input(directory), tag: 'v0.0.6' }),
    /does not match updater version/,
  );
  await assert.rejects(
    buildUpdaterManifest({ ...input(directory), publicationDate: 'July 22, 2026' }),
    /Invalid RFC 3339 publication date/,
  );
});

test('rejects updater artifacts built for a different application version', async t => {
  const directory = await fixture({
    'app_0.0.4_x64-setup.exe': 'installer',
    'app_0.0.4_x64-setup.exe.sig': 'windows-signature',
    'app_0.0.4_amd64.AppImage': 'appimage',
    'app_0.0.4_amd64.AppImage.sig': 'linux-signature',
  });
  t.after(() => rm(directory, { recursive: true, force: true }));

  await assert.rejects(
    buildUpdaterManifest(input(directory)),
    /does not contain release version 0\.0\.5/,
  );
});

test('rejects prerelease and build filenames for a stable release', async t => {
  for (const windowsName of [
    'app_0.0.5-rc.1_x64-setup.exe',
    'app_0.0.5+build.7_x64-setup.exe',
    'app_0.0.5.1_x64-setup.exe',
  ]) {
    const directory = await fixture({
      [windowsName]: 'installer',
      [`${windowsName}.sig`]: 'windows-signature',
      'app_0.0.5_amd64.AppImage': 'appimage',
      'app_0.0.5_amd64.AppImage.sig': 'linux-signature',
    });
    t.after(() => rm(directory, { recursive: true, force: true }));

    await assert.rejects(
      buildUpdaterManifest(input(directory)),
      /does not contain release version 0\.0\.5/,
    );
  }
});

test('requires the declared version immediately before the architecture suffix', async t => {
  const directory = await fixture({
    'app_0.0.5_candidate_x64-setup.exe': 'installer',
    'app_0.0.5_candidate_x64-setup.exe.sig': 'windows-signature',
    'app_0.0.5_amd64.AppImage': 'appimage',
    'app_0.0.5_amd64.AppImage.sig': 'linux-signature',
  });
  t.after(() => rm(directory, { recursive: true, force: true }));

  await assert.rejects(
    buildUpdaterManifest(input(directory)),
    /does not match its exact platform\/version suffix/,
  );
});

test('rejects artifacts for a different CPU architecture', async t => {
  const wrongWindows = await fixture({
    'app_0.0.5_arm64-setup.exe': 'installer',
    'app_0.0.5_arm64-setup.exe.sig': 'windows-signature',
    'app_0.0.5_amd64.AppImage': 'appimage',
    'app_0.0.5_amd64.AppImage.sig': 'linux-signature',
  });
  const wrongLinux = await fixture({
    'app_0.0.5_x64-setup.exe': 'installer',
    'app_0.0.5_x64-setup.exe.sig': 'windows-signature',
    'app_0.0.5_aarch64.AppImage': 'appimage',
    'app_0.0.5_aarch64.AppImage.sig': 'linux-signature',
  });
  t.after(() => rm(wrongWindows, { recursive: true, force: true }));
  t.after(() => rm(wrongLinux, { recursive: true, force: true }));

  await assert.rejects(
    buildUpdaterManifest(input(wrongWindows)),
    /Windows NSIS updater requires exactly one artifact, found 0/,
  );
  await assert.rejects(
    buildUpdaterManifest(input(wrongLinux)),
    /Linux AppImage updater requires exactly one artifact, found 0/,
  );
});
