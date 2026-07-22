#!/usr/bin/env node

import { lstat, readFile, readdir, writeFile } from 'node:fs/promises';
import { basename, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

const SEMVER = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-(?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*)(?:\.(?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*))*)?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$/;
const RFC_3339 = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})$/;
const REPOSITORY = /^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/;
const MAX_SIGNATURE_BYTES = 16 * 1024;

const TARGETS = [
  {
    // Tauri searches the OS/architecture/installer target before its generic
    // fallback, keeping the update format bound to the running bundle.
    key: 'windows-x86_64-nsis',
    description: 'Windows NSIS updater',
    matches: name => name.toLowerCase().endsWith('_x64-setup.exe'),
    expectedSuffix: version => `_${version}_x64-setup.exe`,
  },
  {
    key: 'linux-x86_64-appimage',
    description: 'Linux AppImage updater',
    matches: name => name.endsWith('_amd64.AppImage'),
    expectedSuffix: version => `_${version}_amd64.AppImage`,
  },
];

function validateVersion(version) {
  if (!SEMVER.test(version)) throw new Error(`Invalid semantic version: ${version}`);
}

function validateRepository(repository) {
  if (!REPOSITORY.test(repository)) {
    throw new Error(`Invalid GitHub repository slug: ${repository}`);
  }
}

function validatePublicationDate(publicationDate) {
  if (
    typeof publicationDate !== 'string'
    || !RFC_3339.test(publicationDate)
    || Number.isNaN(Date.parse(publicationDate))
  ) {
    throw new Error(`Invalid RFC 3339 publication date: ${publicationDate}`);
  }
}

function releaseAssetUrl(repository, tag, fileName) {
  const encodedName = encodeURIComponent(fileName);
  return `https://github.com/${repository}/releases/download/${encodeURIComponent(tag)}/${encodedName}`;
}

async function regularFiles(directory) {
  const files = [];
  const visit = async current => {
    const entries = await readdir(current, { withFileTypes: true });
    for (const entry of entries) {
      const path = resolve(current, entry.name);
      if (entry.isDirectory()) {
        await visit(path);
      } else if (entry.isFile()) {
        files.push({ name: entry.name, path });
      } else {
        throw new Error(`Release assets must not contain symbolic links or special files: ${path}`);
      }
    }
  };
  await visit(directory);
  return files.sort((left, right) => left.path.localeCompare(right.path));
}

function containsVersion(fileName, version) {
  let start = fileName.indexOf(version);
  while (start !== -1) {
    const before = start === 0 ? '' : fileName[start - 1];
    const after = fileName.slice(start + version.length);
    const firstAfter = after[0] ?? '';
    const invalidAfter = /[0-9A-Za-z+-]/.test(firstAfter)
      || (firstAfter === '.' && /[0-9]/.test(after[1] ?? ''));
    if (!/[0-9A-Za-z]/.test(before) && !invalidAfter) return true;
    start = fileName.indexOf(version, start + 1);
  }
  return false;
}

async function requireOneArtifact(files, target, version) {
  const matches = files.filter(file => target.matches(file.name));
  if (matches.length !== 1) {
    throw new Error(
      `${target.description} requires exactly one artifact, found ${matches.length}: ${matches.map(file => file.name).join(', ') || 'none'}`,
    );
  }

  const artifact = matches[0];
  const artifactName = artifact.name;
  if (!containsVersion(artifactName, version)) {
    throw new Error(
      `${target.description} artifact name does not contain release version ${version}: ${artifactName}`,
    );
  }
  if (!artifactName.endsWith(target.expectedSuffix(version))) {
    throw new Error(
      `${target.description} artifact name does not match its exact platform/version suffix: ${artifactName}`,
    );
  }
  const artifactStat = await lstat(artifact.path);
  if (!artifactStat.isFile() || artifactStat.size === 0) {
    throw new Error(`${target.description} artifact is empty or not a regular file: ${artifactName}`);
  }

  const signatureName = `${artifactName}.sig`;
  const signaturePath = `${artifact.path}.sig`;
  if (!files.some(file => file.path === signaturePath)) {
    throw new Error(`${target.description} is missing its Tauri signature: ${signatureName}`);
  }

  const signatureStat = await lstat(signaturePath);
  if (!signatureStat.isFile() || signatureStat.size === 0 || signatureStat.size > MAX_SIGNATURE_BYTES) {
    throw new Error(`${target.description} signature has an invalid size: ${signatureName}`);
  }

  const signature = (await readFile(signaturePath, 'utf8')).trim();
  if (!signature || signature.includes('\0')) {
    throw new Error(`${target.description} signature is empty or malformed: ${signatureName}`);
  }

  return { artifactName, signature };
}

export async function buildUpdaterManifest({
  assetsDirectory,
  version,
  tag = `v${version}`,
  repository,
  notes,
  publicationDate,
}) {
  validateVersion(version);
  validateRepository(repository);
  validatePublicationDate(publicationDate);
  if (tag !== `v${version}`) {
    throw new Error(`Release tag ${tag} does not match updater version v${version}`);
  }
  if (typeof notes !== 'string' || !notes.trim()) {
    throw new Error('Bilingual release notes must not be empty');
  }

  const directory = resolve(assetsDirectory);
  const files = await regularFiles(directory);
  const platforms = {};

  for (const target of TARGETS) {
    const { artifactName, signature } = await requireOneArtifact(files, target, version);
    platforms[target.key] = {
      signature,
      url: releaseAssetUrl(repository, tag, artifactName),
    };
  }

  return {
    version,
    notes: notes.trim(),
    pub_date: publicationDate,
    platforms,
  };
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
  for (const required of ['assets-dir', 'version', 'tag', 'repository', 'notes-file', 'output']) {
    if (!values[required]) throw new Error(`Missing required --${required}`);
  }
  return values;
}

export async function runCli(argv = process.argv.slice(2)) {
  const args = parseArguments(argv);
  const notes = await readFile(resolve(args['notes-file']), 'utf8');
  const manifest = await buildUpdaterManifest({
    assetsDirectory: args['assets-dir'],
    version: args.version,
    tag: args.tag,
    repository: args.repository,
    notes,
    publicationDate: args['pub-date'] ?? new Date().toISOString(),
  });
  const output = resolve(args.output);
  await writeFile(output, `${JSON.stringify(manifest, null, 2)}\n`, { encoding: 'utf8', flag: 'w' });
  return output;
}

const invokedPath = process.argv[1] ? pathToFileURL(resolve(process.argv[1])).href : '';
if (import.meta.url === invokedPath) {
  runCli()
    .then(output => process.stdout.write(`Wrote updater manifest for signed artifacts to ${basename(output)}\n`))
    .catch(error => {
      process.stderr.write(`${error instanceof Error ? error.message : String(error)}\n`);
      process.exitCode = 1;
    });
}
