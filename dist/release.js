'use strict';
// Finds and downloads the prebuilt unstuck-proxy of a release. Plain Node.js,
// no dependencies; kept apart from main.js so it can be unit tested.

const fs = require('fs');
const os = require('os');
const path = require('path');
const crypto = require('crypto');
const { sleep } = require('./common');

const ARCH = { x64: 'x86_64', arm64: 'aarch64' }[process.arch];
const SHA_RE = /^[0-9a-f]{40}$/;
const RELEASE_TAG_RE = /^v(\d+)\.(\d+)\.(\d+)(-.*)?$/;

function tempRoot() {
  return process.env.RUNNER_TEMP || os.tmpdir();
}

async function fetchWithRetry(url, init = {}) {
  let lastError;
  for (let i = 0; i < 3; i++) {
    try {
      const r = await fetch(url, { redirect: 'follow', ...init });
      if (r.ok || r.status === 404) return r;
      lastError = new Error(`${url}: HTTP ${r.status}`);
    } catch (e) {
      lastError = e;
    }
    await sleep(2000 * (i + 1));
  }
  throw lastError;
}

// Sort key for release tags: newest first, a final release before its pre-releases.
function releaseOrder(a, b) {
  const [, ...pa] = a.match(RELEASE_TAG_RE);
  const [, ...pb] = b.match(RELEASE_TAG_RE);
  for (let i = 0; i < 3; i++) {
    const d = Number(pb[i]) - Number(pa[i]);
    if (d) return d;
  }
  if (!pa[3] !== !pb[3]) return pa[3] ? 1 : -1;
  return (pb[3] || '').localeCompare(pa[3] || '');
}

// The release tag (vX.Y.Z) that points at the commit, or null when there is
// none. Floating tags like vX are not releases of a specific version.
async function resolveReleaseTag(repo, sha, { token = '', apiUrl } = {}) {
  const api = apiUrl || process.env.GITHUB_API_URL || 'https://api.github.com';
  const headers = { Accept: 'application/vnd.github+json', 'X-GitHub-Api-Version': '2022-11-28' };
  if (token) headers.Authorization = `Bearer ${token}`;
  const found = [];
  for (let page = 1; page <= 10; page++) {
    const r = await fetchWithRetry(`${api}/repos/${repo}/tags?per_page=100&page=${page}`, { headers });
    if (r.status === 404) throw new Error(`cannot list tags of ${repo}: HTTP 404`);
    const tags = await r.json();
    for (const t of tags) {
      if (RELEASE_TAG_RE.test(t.name) && t.commit && t.commit.sha === sha) found.push(t.name);
    }
    if (tags.length < 100) break;
  }
  return found.sort(releaseOrder)[0] || null;
}

// Downloads the release binary for the tag. Returns null when the tag has no
// release, so the caller can build from source instead.
async function downloadRelease(repo, tag) {
  if (!ARCH) throw new Error(`unsupported architecture ${process.arch}`);
  const server = process.env.GITHUB_SERVER_URL || 'https://github.com';
  const base = `${server}/${repo}/releases/download/${encodeURIComponent(tag)}`;
  const asset = `unstuck-proxy-${ARCH}-linux`;
  const sums = await fetchWithRetry(`${base}/SHA256SUMS`);
  if (sums.status === 404) return null;
  const expected = (await sums.text())
    .split('\n')
    .map((l) => l.trim().split(/\s+/))
    .find((f) => f[1] && f[1].replace(/^\*/, '') === asset);
  if (!expected) throw new Error(`SHA256SUMS of ${tag} has no entry for ${asset}`);
  const bin = await fetchWithRetry(`${base}/${asset}`);
  if (bin.status === 404) throw new Error(`release ${tag} has no ${asset}`);
  const data = Buffer.from(await bin.arrayBuffer());
  const actual = crypto.createHash('sha256').update(data).digest('hex');
  if (actual !== expected[0].toLowerCase()) {
    throw new Error(`checksum mismatch for ${asset}: expected ${expected[0]}, got ${actual}`);
  }
  const dir = path.join(tempRoot(), 'unstuck-proxy-bin', tag.replace(/[^\w.-]/g, '_'));
  fs.mkdirSync(dir, { recursive: true });
  const dest = path.join(dir, 'unstuck-proxy');
  fs.writeFileSync(dest, data, { mode: 0o755 });
  console.log(`Downloaded ${asset} from release ${tag} (sha256 ${actual})`);
  return dest;
}

// The prebuilt binary for the action's own ref: a release tag, or a commit
// SHA that a release tag points at. Returns null when the ref has no release.
async function downloadForRef(repo, ref, { token = '' } = {}) {
  let tag = ref;
  if (SHA_RE.test(ref)) {
    tag = await resolveReleaseTag(repo, ref, { token });
    if (!tag) {
      console.log(`No release is tagged at commit ${ref}`);
      return null;
    }
    console.log(`Commit ${ref} is release ${tag}`);
  }
  return downloadRelease(repo, tag);
}

module.exports = { fetchWithRetry, resolveReleaseTag, downloadRelease, downloadForRef, SHA_RE };
