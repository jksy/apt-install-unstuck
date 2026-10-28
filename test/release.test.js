'use strict';
// node --test test/
const { test, beforeEach, afterEach } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('fs');
const os = require('os');
const path = require('path');
const crypto = require('crypto');
const release = require('../dist/release');

const SHA = 'ec484ecdc5f2b2621ac15508deec76003b1ea751';
const OTHER = '0000000000000000000000000000000000000000';
const tag = (name, sha) => ({ name, commit: { sha } });

// Replaces global fetch with a table of URL (without query) -> response.
let requests;
function mockFetch(routes) {
  requests = [];
  globalThis.fetch = async (url, init = {}) => {
    requests.push({ url, init });
    const u = new URL(url);
    const page = Number(u.searchParams.get('page') || 1);
    const route = routes[u.origin + u.pathname];
    if (!route) return new Response('not found', { status: 404 });
    const body = typeof route === 'function' ? route(page) : route;
    if (body instanceof Response) return body;
    return new Response(typeof body === 'string' ? body : JSON.stringify(body), { status: 200 });
  };
}

const realFetch = globalThis.fetch;
let tmp;
beforeEach(() => {
  tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'release-test-'));
  process.env.RUNNER_TEMP = tmp;
  process.env.GITHUB_API_URL = 'https://api.example';
  process.env.GITHUB_SERVER_URL = 'https://example';
});
afterEach(() => {
  globalThis.fetch = realFetch;
  fs.rmSync(tmp, { recursive: true, force: true });
});

const TAGS = 'https://api.example/repos/o/r/tags';

test('resolveReleaseTag: picks the release tag at the commit, not the floating tag', async () => {
  mockFetch({ [TAGS]: [tag('v0', SHA), tag('v0.1.0', SHA), tag('v0.0.9', OTHER)] });
  assert.equal(await release.resolveReleaseTag('o/r', SHA), 'v0.1.0');
});

test('resolveReleaseTag: null when no release tag points at the commit', async () => {
  mockFetch({ [TAGS]: [tag('v0', SHA), tag('v0.1.0', OTHER)] });
  assert.equal(await release.resolveReleaseTag('o/r', SHA), null);
});

test('resolveReleaseTag: newest version wins, final release before pre-release', async () => {
  mockFetch({ [TAGS]: [tag('v0.2.0-rc1', SHA), tag('v0.1.1', SHA), tag('v0.2.0', SHA), tag('v0.1.2', SHA)] });
  assert.equal(await release.resolveReleaseTag('o/r', SHA), 'v0.2.0');
  mockFetch({ [TAGS]: [tag('v0.2.0-rc1', SHA), tag('v0.1.2', SHA)] });
  assert.equal(await release.resolveReleaseTag('o/r', SHA), 'v0.2.0-rc1');
});

test('resolveReleaseTag: follows pagination', async () => {
  const page1 = Array.from({ length: 100 }, (_, i) => tag(`v0.0.${i}`, OTHER));
  mockFetch({ [TAGS]: (page) => (page === 1 ? page1 : page === 2 ? [tag('v1.0.0', SHA)] : []) });
  assert.equal(await release.resolveReleaseTag('o/r', SHA), 'v1.0.0');
  assert.deepEqual(requests.map((r) => new URL(r.url).searchParams.get('page')), ['1', '2']);
});

test('resolveReleaseTag: sends the token as a bearer header, none without a token', async () => {
  mockFetch({ [TAGS]: [] });
  await release.resolveReleaseTag('o/r', SHA, { token: 'ghs_x' });
  assert.equal(requests[0].init.headers.Authorization, 'Bearer ghs_x');
  await release.resolveReleaseTag('o/r', SHA);
  assert.equal(requests[1].init.headers.Authorization, undefined);
});

test('resolveReleaseTag: an unknown repository is an error, not a missing release', async () => {
  mockFetch({});
  await assert.rejects(release.resolveReleaseTag('o/r', SHA), /cannot list tags of o\/r/);
});

function releaseRoutes(tagName, data, sums) {
  const base = `https://example/o/r/releases/download/${tagName}`;
  const asset = `unstuck-proxy-${process.arch === 'arm64' ? 'aarch64' : 'x86_64'}-linux`;
  const sha256 = crypto.createHash('sha256').update(data).digest('hex');
  return {
    [`${base}/SHA256SUMS`]: sums === undefined ? `${sha256}  ${asset}\n` : sums,
    [`${base}/${asset}`]: data,
  };
}

test('downloadRelease: verifies the checksum and installs the binary', async () => {
  mockFetch(releaseRoutes('v0.1.0', 'binary-bytes'));
  const bin = await release.downloadRelease('o/r', 'v0.1.0');
  assert.equal(fs.readFileSync(bin, 'utf8'), 'binary-bytes');
  assert.equal(fs.statSync(bin).mode & 0o111, 0o111);
  assert.ok(bin.startsWith(tmp));
});

test('downloadRelease: null when the tag has no release', async () => {
  mockFetch({});
  assert.equal(await release.downloadRelease('o/r', 'v9.9.9'), null);
});

test('downloadRelease: rejects a checksum mismatch', async () => {
  mockFetch(releaseRoutes('v0.1.0', 'binary-bytes', `${'0'.repeat(64)}  unstuck-proxy-x86_64-linux\n${'0'.repeat(64)}  unstuck-proxy-aarch64-linux\n`));
  await assert.rejects(release.downloadRelease('o/r', 'v0.1.0'), /checksum mismatch/);
});

test('downloadForRef: a commit SHA of a release downloads that release', async () => {
  mockFetch({ [TAGS]: [tag('v0.1.0', SHA)], ...releaseRoutes('v0.1.0', 'binary-bytes') });
  const bin = await release.downloadForRef('o/r', SHA, { token: 't' });
  assert.equal(fs.readFileSync(bin, 'utf8'), 'binary-bytes');
});

test('downloadForRef: a commit SHA without a release is null, with no download attempted', async () => {
  mockFetch({ [TAGS]: [tag('v0.1.0', OTHER)] });
  assert.equal(await release.downloadForRef('o/r', SHA), null);
  assert.equal(requests.length, 1);
});

test('downloadForRef: a tag is used as the release name without a lookup', async () => {
  mockFetch(releaseRoutes('v0', 'binary-bytes'));
  const bin = await release.downloadForRef('o/r', 'v0');
  assert.equal(fs.readFileSync(bin, 'utf8'), 'binary-bytes');
  assert.ok(requests.every((r) => !r.url.includes('/tags')));
});
