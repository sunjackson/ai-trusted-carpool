import assert from 'node:assert/strict'
import test from 'node:test'

import { classifyReleaseRef } from './classify-release-ref.mjs'

const appVersion = '0.0.5'

test('classifies branches and pull requests as development builds', () => {
  assert.equal(
    classifyReleaseRef({ ref: 'refs/heads/main', refName: 'main', appVersion }),
    'development',
  )
  assert.equal(
    classifyReleaseRef({
      ref: 'refs/pull/42/merge',
      refName: '42/merge',
      appVersion,
    }),
    'development',
  )
})

test('classifies an exact version tag as a signed release', () => {
  assert.equal(
    classifyReleaseRef({
      ref: 'refs/tags/v0.0.5',
      refName: 'v0.0.5',
      appVersion,
    }),
    'signed',
  )
})

test('classifies an explicit positive test sequence as a test prerelease', () => {
  assert.equal(
    classifyReleaseRef({
      ref: 'refs/tags/v0.0.5-test.1',
      refName: 'v0.0.5-test.1',
      appVersion,
    }),
    'test',
  )
  assert.equal(
    classifyReleaseRef({
      ref: 'refs/tags/v0.0.5-test.27',
      refName: 'v0.0.5-test.27',
      appVersion,
    }),
    'test',
  )
})

test('rejects ambiguous or mismatched release tags', () => {
  for (const refName of [
    'v0.0.4',
    'v0.0.5-test',
    'v0.0.5-test.0',
    'v0.0.5-test.01',
    'v0.0.5-rc.1',
  ]) {
    assert.throws(
      () =>
        classifyReleaseRef({
          ref: `refs/tags/${refName}`,
          refName,
          appVersion,
        }),
      /must be v0\.0\.5 or v0\.0\.5-test\.N/,
    )
  }
})
