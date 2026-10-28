'use strict';
// E2E for SHA pinning: the commit of a released tag must resolve to that
// release, and its binary must download and run. Usage:
//   GITHUB_TOKEN=... node .github/e2e/resolve-sha.js <owner/repo> <sha>
const { spawnSync } = require('child_process');
const release = require('../../dist/release');

(async () => {
  const [repo, sha] = process.argv.slice(2);
  const tag = await release.resolveReleaseTag(repo, sha, { token: process.env.GITHUB_TOKEN });
  if (!tag) throw new Error(`no release tag points at ${sha}`);
  console.log(`${sha} -> ${tag}`);
  const bin = await release.downloadForRef(repo, sha, { token: process.env.GITHUB_TOKEN });
  if (!bin) throw new Error(`no release binary for ${sha}`);
  const r = spawnSync(bin, ['--version'], { encoding: 'utf8' });
  if (r.status !== 0) throw new Error(`binary does not run: ${r.stderr}`);
  console.log(r.stdout.trim());
})().catch((e) => {
  console.error(`::error::${e.message}`);
  process.exit(1);
});
