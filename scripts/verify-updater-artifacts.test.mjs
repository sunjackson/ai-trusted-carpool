import assert from 'node:assert/strict';
import {
  createHash,
  generateKeyPairSync,
  sign,
} from 'node:crypto';
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';

import {
  EXPECTED_UPDATER_PUBKEY_CONFIG_BASE64_SHA256,
  verifyUpdaterArtifacts,
} from './verify-updater-artifacts.mjs';

const KEY_ID = Buffer.from('0102030405060708', 'hex');
const PUBLIC_KEY_ALGORITHM = Buffer.from([0x45, 0x64]);
const PREHASHED_ALGORITHM = Buffer.from([0x45, 0x44]);

async function fixture({
  artifact = Buffer.from('signed updater artifact'),
  artifactName = 'trusted-carpool_0.0.5_x64-setup.exe',
  signWithDifferentKey = false,
  signedFileName = artifactName,
} = {}) {
  const directory = await mkdtemp(join(tmpdir(), 'trusted-carpool-signature-'));
  const { privateKey, publicKey } = generateKeyPairSync('ed25519');
  const signingKey = signWithDifferentKey
    ? generateKeyPairSync('ed25519').privateKey
    : privateKey;
  const publicJwk = publicKey.export({ format: 'jwk' });
  const publicPayload = Buffer.concat([
    PUBLIC_KEY_ALGORITHM,
    KEY_ID,
    Buffer.from(publicJwk.x, 'base64url'),
  ]);
  const publicKeyText = [
    'untrusted comment: minisign public key: fixture',
    publicPayload.toString('base64'),
    '',
  ].join('\n');
  const encodedPublicKey = Buffer.from(publicKeyText).toString('base64');
  const expectedConfigBase64Sha256 = createHash('sha256')
    .update(encodedPublicKey)
    .digest('hex');
  const configPath = join(directory, 'tauri.conf.json');
  await writeFile(
    configPath,
    JSON.stringify({ plugins: { updater: { pubkey: encodedPublicKey } } }),
  );

  const digest = createHash('blake2b512').update(artifact).digest();
  const primarySignature = sign(null, digest, signingKey);
  const trustedComment = `timestamp:1784695535\tfile:${signedFileName}`;
  const globalSignature = sign(
    null,
    Buffer.concat([primarySignature, Buffer.from(trustedComment)]),
    signingKey,
  );
  const signaturePayload = Buffer.concat([
    PREHASHED_ALGORITHM,
    KEY_ID,
    primarySignature,
  ]);
  const signatureText = [
    'untrusted comment: signature from tauri secret key',
    signaturePayload.toString('base64'),
    `trusted comment: ${trustedComment}`,
    globalSignature.toString('base64'),
    '',
  ].join('\n');
  const artifactPath = join(directory, artifactName);
  await writeFile(artifactPath, artifact);
  await writeFile(`${artifactPath}.sig`, Buffer.from(signatureText).toString('base64'));

  return {
    artifactPath,
    configPath,
    directory,
    expectedConfigBase64Sha256,
  };
}

test('cryptographically verifies a Tauri updater artifact and signed filename', async t => {
  const data = await fixture();
  t.after(() => rm(data.directory, { recursive: true, force: true }));

  const result = await verifyUpdaterArtifacts({
    assetsDirectory: data.directory,
    configPath: data.configPath,
    expectedConfigBase64Sha256: data.expectedConfigBase64Sha256,
  });

  assert.deepEqual(result.artifacts, [data.artifactPath]);
  assert.equal(
    result.publicKeyConfigBase64Sha256,
    data.expectedConfigBase64Sha256,
  );
});

test('fails closed when artifact bytes change after signing', async t => {
  const data = await fixture();
  t.after(() => rm(data.directory, { recursive: true, force: true }));
  await writeFile(data.artifactPath, 'tampered updater artifact');

  await assert.rejects(
    verifyUpdaterArtifacts({
      assetsDirectory: data.directory,
      configPath: data.configPath,
      expectedConfigBase64Sha256: data.expectedConfigBase64Sha256,
    }),
    /artifact does not match its embedded-key signature/,
  );
});

test('fails closed when CI uses a signing key other than the embedded key', async t => {
  const data = await fixture({ signWithDifferentKey: true });
  t.after(() => rm(data.directory, { recursive: true, force: true }));

  await assert.rejects(
    verifyUpdaterArtifacts({
      assetsDirectory: data.directory,
      configPath: data.configPath,
      expectedConfigBase64Sha256: data.expectedConfigBase64Sha256,
    }),
    /signature metadata does not match the embedded public key/,
  );
});

test('rejects signed metadata that names a different artifact', async t => {
  const data = await fixture({ signedFileName: 'other_0.0.5_x64-setup.exe' });
  t.after(() => rm(data.directory, { recursive: true, force: true }));

  await assert.rejects(
    verifyUpdaterArtifacts({
      assetsDirectory: data.directory,
      configPath: data.configPath,
      expectedConfigBase64Sha256: data.expectedConfigBase64Sha256,
    }),
    /signature names other_0\.0\.5_x64-setup\.exe/,
  );
});

test('pins the updater public key embedded in the Tauri config', async () => {
  const config = JSON.parse(await readFile('src-tauri/tauri.conf.json', 'utf8'));
  const actual = createHash('sha256')
    .update(config.plugins.updater.pubkey)
    .digest('hex');
  assert.equal(actual, EXPECTED_UPDATER_PUBKEY_CONFIG_BASE64_SHA256);
});
