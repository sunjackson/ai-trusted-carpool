#!/usr/bin/env node

import {
  createHash,
  createPublicKey,
  verify as verifyEd25519,
} from 'node:crypto';
import { createReadStream } from 'node:fs';
import { lstat, readFile, readdir } from 'node:fs/promises';
import { basename, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import { TextDecoder } from 'node:util';

export const EXPECTED_UPDATER_PUBKEY_CONFIG_BASE64_SHA256 =
  'e773d48d10f364b1f38b827109e7c4a2f5203e61561ec89bd1e2da94b1e7170d';

const MAX_SIGNATURE_BYTES = 16 * 1024;
const PUBLIC_KEY_BYTES = 42;
const SIGNATURE_BYTES = 74;
const GLOBAL_SIGNATURE_BYTES = 64;
const PREHASHED_ALGORITHM = Buffer.from([0x45, 0x44]);
const SUPPORTED_PUBLIC_KEY_ALGORITHMS = new Set(['4564', '4544']);
const TRUSTED_COMMENT_PREFIX = 'trusted comment: ';
const utf8 = new TextDecoder('utf-8', { fatal: true });

function decodeBase64(value, label) {
  if (
    typeof value !== 'string'
    || value.length === 0
    || value.length % 4 !== 0
    || !/^[A-Za-z0-9+/]+={0,2}$/.test(value)
  ) {
    throw new Error(`${label} is not canonical base64`);
  }
  const decoded = Buffer.from(value, 'base64');
  if (decoded.toString('base64') !== value) {
    throw new Error(`${label} is not canonical base64`);
  }
  return decoded;
}

function decodeUtf8(value, label) {
  try {
    return utf8.decode(value);
  } catch {
    throw new Error(`${label} is not valid UTF-8`);
  }
}

function parsePublicKey(encodedPublicKey, expectedConfigBase64Sha256) {
  const actualHash = createHash('sha256').update(encodedPublicKey).digest('hex');
  if (actualHash !== expectedConfigBase64Sha256) {
    throw new Error(
      `Embedded updater public-key config value SHA-256 ${actualHash} does not match pinned ${expectedConfigBase64Sha256}`,
    );
  }

  const publicKeyText = decodeUtf8(
    decodeBase64(encodedPublicKey, 'Embedded updater public key'),
    'Embedded updater public key',
  );
  const lines = publicKeyText.trimEnd().split(/\r?\n/);
  if (lines.length !== 2 || !lines[0].startsWith('untrusted comment: ')) {
    throw new Error('Embedded updater public key has an invalid Minisign envelope');
  }

  const payload = decodeBase64(lines[1], 'Embedded Minisign public key payload');
  if (
    payload.length !== PUBLIC_KEY_BYTES
    || !SUPPORTED_PUBLIC_KEY_ALGORITHMS.has(payload.subarray(0, 2).toString('hex'))
  ) {
    throw new Error('Embedded updater public key uses an invalid Minisign format');
  }

  const keyId = payload.subarray(2, 10);
  const publicKey = createPublicKey({
    format: 'jwk',
    key: {
      crv: 'Ed25519',
      kty: 'OKP',
      x: payload.subarray(10).toString('base64url'),
    },
  });
  return { actualHash, keyId, publicKey };
}

function parseSignature(encodedSignature, signaturePath) {
  const signatureText = decodeUtf8(
    decodeBase64(encodedSignature, `Updater signature ${signaturePath}`),
    `Updater signature ${signaturePath}`,
  );
  const lines = signatureText.trimEnd().split(/\r?\n/);
  if (
    lines.length !== 4
    || !lines[0].startsWith('untrusted comment: ')
    || !lines[2].startsWith(TRUSTED_COMMENT_PREFIX)
  ) {
    throw new Error(`Updater signature ${signaturePath} has an invalid Minisign envelope`);
  }

  const payload = decodeBase64(lines[1], `Updater signature payload ${signaturePath}`);
  const globalSignature = decodeBase64(
    lines[3],
    `Updater global signature ${signaturePath}`,
  );
  if (
    payload.length !== SIGNATURE_BYTES
    || !payload.subarray(0, 2).equals(PREHASHED_ALGORITHM)
    || globalSignature.length !== GLOBAL_SIGNATURE_BYTES
  ) {
    throw new Error(`Updater signature ${signaturePath} is not a prehashed Minisign signature`);
  }

  const trustedComment = lines[2].slice(TRUSTED_COMMENT_PREFIX.length);
  const timestamp = trustedComment.match(/(?:^|\t)timestamp:(\d+)(?:\t|$)/);
  const signedFile = trustedComment.match(/(?:^|\t)file:([^\t\r\n]+)(?:\t|$)/);
  if (!timestamp || !signedFile) {
    throw new Error(`Updater signature ${signaturePath} is missing signed file metadata`);
  }

  return {
    fileName: signedFile[1],
    globalSignature,
    keyId: payload.subarray(2, 10),
    primarySignature: payload.subarray(10),
    trustedComment,
  };
}

async function signatureFiles(directory) {
  const found = [];
  const visit = async current => {
    const entries = await readdir(current, { withFileTypes: true });
    for (const entry of entries) {
      const path = resolve(current, entry.name);
      if (entry.isDirectory()) {
        await visit(path);
      } else if (entry.name.endsWith('.sig')) {
        if (!entry.isFile()) {
          throw new Error(`Updater signature must be a regular file: ${path}`);
        }
        found.push(path);
      }
    }
  };
  await visit(resolve(directory));
  return found.sort();
}

async function blake2b512(path) {
  const hash = createHash('blake2b512');
  for await (const chunk of createReadStream(path)) hash.update(chunk);
  return hash.digest();
}

async function verifyPair(signaturePath, key) {
  const signatureStat = await lstat(signaturePath);
  if (
    !signatureStat.isFile()
    || signatureStat.size === 0
    || signatureStat.size > MAX_SIGNATURE_BYTES
  ) {
    throw new Error(`Updater signature has an invalid size: ${signaturePath}`);
  }

  const artifactPath = signaturePath.slice(0, -'.sig'.length);
  const artifactStat = await lstat(artifactPath).catch(() => null);
  if (!artifactStat?.isFile() || artifactStat.size === 0) {
    throw new Error(`Updater signature has no non-empty regular artifact: ${signaturePath}`);
  }

  const encodedSignature = (await readFile(signaturePath, 'utf8')).trim();
  const signature = parseSignature(encodedSignature, signaturePath);
  if (!signature.keyId.equals(key.keyId)) {
    throw new Error(`Updater signature was produced by a different key: ${signaturePath}`);
  }
  if (signature.fileName !== basename(artifactPath)) {
    throw new Error(
      `Updater signature names ${signature.fileName}, not ${basename(artifactPath)}: ${signaturePath}`,
    );
  }

  const globalMessage = Buffer.concat([
    signature.primarySignature,
    Buffer.from(signature.trustedComment, 'utf8'),
  ]);
  if (
    !verifyEd25519(
      null,
      globalMessage,
      key.publicKey,
      signature.globalSignature,
    )
  ) {
    throw new Error(`Updater signature metadata does not match the embedded public key: ${signaturePath}`);
  }

  const digest = await blake2b512(artifactPath);
  if (!verifyEd25519(null, digest, key.publicKey, signature.primarySignature)) {
    throw new Error(`Updater artifact does not match its embedded-key signature: ${artifactPath}`);
  }
  return artifactPath;
}

export async function verifyUpdaterArtifacts({
  assetsDirectory,
  configPath = 'src-tauri/tauri.conf.json',
  expectedConfigBase64Sha256 = EXPECTED_UPDATER_PUBKEY_CONFIG_BASE64_SHA256,
}) {
  const config = JSON.parse(await readFile(resolve(configPath), 'utf8'));
  const encodedPublicKey = config?.plugins?.updater?.pubkey;
  if (typeof encodedPublicKey !== 'string' || encodedPublicKey.length === 0) {
    throw new Error('Tauri config does not contain an updater public key');
  }
  const key = parsePublicKey(encodedPublicKey, expectedConfigBase64Sha256);
  const signatures = await signatureFiles(assetsDirectory);
  if (signatures.length === 0) {
    throw new Error(`No updater signatures found under ${resolve(assetsDirectory)}`);
  }

  const artifacts = [];
  for (const signaturePath of signatures) {
    artifacts.push(await verifyPair(signaturePath, key));
  }
  return { artifacts, publicKeyConfigBase64Sha256: key.actualHash };
}

function parseArguments(argv) {
  const values = {};
  for (let index = 0; index < argv.length; index += 1) {
    const key = argv[index];
    if (!key.startsWith('--')) throw new Error(`Unknown argument: ${key}`);
    const value = argv[index + 1];
    if (!value || value.startsWith('--')) throw new Error(`Missing value for ${key}`);
    values[key.slice(2)] = value;
    index += 1;
  }
  if (!values['assets-dir']) throw new Error('Missing required --assets-dir');
  return values;
}

export async function runCli(argv = process.argv.slice(2)) {
  const args = parseArguments(argv);
  return verifyUpdaterArtifacts({
    assetsDirectory: args['assets-dir'],
    configPath: args.config ?? 'src-tauri/tauri.conf.json',
  });
}

const invokedPath = process.argv[1] ? pathToFileURL(resolve(process.argv[1])).href : '';
if (import.meta.url === invokedPath) {
  runCli()
    .then(result => {
      process.stdout.write(
        `Verified ${result.artifacts.length} updater artifact signature(s) with embedded public-key config value ${result.publicKeyConfigBase64Sha256}.\n`,
      );
    })
    .catch(error => {
      process.stderr.write(`${error instanceof Error ? error.message : String(error)}\n`);
      process.exitCode = 1;
    });
}
