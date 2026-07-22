import { pathToFileURL } from 'node:url'

export function classifyReleaseRef({
  ref,
  refName,
  appVersion,
  signedReleasesEnabled = false,
}) {
  if (!ref.startsWith('refs/tags/v')) {
    return 'development'
  }

  if (refName === `v${appVersion}`) {
    return signedReleasesEnabled ? 'signed' : 'unsigned'
  }

  const testPrefix = `v${appVersion}-test.`
  const testSequence = refName.startsWith(testPrefix)
    ? refName.slice(testPrefix.length)
    : ''
  if (/^[1-9]\d*$/.test(testSequence)) {
    return 'test'
  }

  throw new Error(
    `Tag ${refName} must be v${appVersion} or v${appVersion}-test.N`,
  )
}

function readArgument(name) {
  const index = process.argv.indexOf(name)
  if (index === -1 || !process.argv[index + 1]) {
    throw new Error(`Missing required argument ${name}`)
  }
  return process.argv[index + 1]
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  try {
    console.log(
      classifyReleaseRef({
        ref: readArgument('--ref'),
        refName: readArgument('--ref-name'),
        appVersion: readArgument('--version'),
        signedReleasesEnabled: process.argv.includes('--signed-release'),
      }),
    )
  } catch (error) {
    console.error(error instanceof Error ? error.message : String(error))
    process.exitCode = 1
  }
}
