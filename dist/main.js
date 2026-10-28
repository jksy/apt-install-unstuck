'use strict';
// Starts unstuck-proxy, runs apt-get through it, and stops it unless keep-running is set.
// Plain Node.js with no dependencies, so the action needs no bundling.

const fs = require('fs');
const os = require('os');
const path = require('path');
const http = require('http');
const crypto = require('crypto');
const { spawn, spawnSync } = require('child_process');
const c = require('./common');

const ACTION_ROOT = path.resolve(__dirname, '..');
const ARCH = { x64: 'x86_64', arm64: 'aarch64' }[process.arch];

function tempRoot() {
  return process.env.RUNNER_TEMP || os.tmpdir();
}

async function fetchWithRetry(url) {
  let lastError;
  for (let i = 0; i < 3; i++) {
    try {
      const r = await fetch(url, { redirect: 'follow' });
      if (r.ok || r.status === 404) return r;
      lastError = new Error(`${url}: HTTP ${r.status}`);
    } catch (e) {
      lastError = e;
    }
    await c.sleep(2000 * (i + 1));
  }
  throw lastError;
}

// Downloads the release binary for the action's own ref. Returns null when
// that ref has no release (a branch or commit), so the caller can build instead.
async function downloadRelease(repo, ref) {
  const server = process.env.GITHUB_SERVER_URL || 'https://github.com';
  const base = `${server}/${repo}/releases/download/${encodeURIComponent(ref)}`;
  const asset = `unstuck-proxy-${ARCH}-linux`;
  const sums = await fetchWithRetry(`${base}/SHA256SUMS`);
  if (sums.status === 404) return null;
  const expected = (await sums.text())
    .split('\n')
    .map((l) => l.trim().split(/\s+/))
    .find((f) => f[1] && f[1].replace(/^\*/, '') === asset);
  if (!expected) throw new Error(`SHA256SUMS of ${ref} has no entry for ${asset}`);
  const bin = await fetchWithRetry(`${base}/${asset}`);
  if (bin.status === 404) throw new Error(`release ${ref} has no ${asset}`);
  const data = Buffer.from(await bin.arrayBuffer());
  const actual = crypto.createHash('sha256').update(data).digest('hex');
  if (actual !== expected[0].toLowerCase()) {
    throw new Error(`checksum mismatch for ${asset}: expected ${expected[0]}, got ${actual}`);
  }
  const dir = path.join(tempRoot(), 'unstuck-proxy-bin', ref.replace(/[^\w.-]/g, '_'));
  fs.mkdirSync(dir, { recursive: true });
  const dest = path.join(dir, 'unstuck-proxy');
  fs.writeFileSync(dest, data, { mode: 0o755 });
  console.log(`Downloaded ${asset} from release ${ref} (sha256 ${actual})`);
  return dest;
}

function buildFromSource() {
  const manifest = path.join(ACTION_ROOT, 'proxy', 'Cargo.toml');
  console.log(`::group::Building unstuck-proxy from source (${manifest})`);
  const r = spawnSync('cargo', ['build', '--release', '--locked', '--manifest-path', manifest], { stdio: 'inherit' });
  console.log('::endgroup::');
  if (r.error) throw new Error(`cannot run cargo to build unstuck-proxy: ${r.error.message}`);
  if (r.status !== 0) throw new Error(`cargo build failed with exit code ${r.status}`);
  return path.join(ACTION_ROOT, 'proxy', 'target', 'release', 'unstuck-proxy');
}

async function resolveBinary() {
  // Escape hatch for testing this action with a prebuilt binary.
  if (process.env.UNSTUCK_PROXY_BIN) return path.resolve(process.env.UNSTUCK_PROXY_BIN);
  if (!ARCH) throw new Error(`unsupported architecture ${process.arch}`);
  const repo = process.env.GITHUB_ACTION_REPOSITORY;
  const ref = process.env.GITHUB_ACTION_REF;
  if (repo && ref) {
    const bin = await downloadRelease(repo, ref);
    if (bin) return bin;
    console.log(`No release for ${repo}@${ref}; building from source`);
  }
  return buildFromSource();
}

function defaultCachePrefix() {
  const arch = (process.env.RUNNER_ARCH || process.arch).toLowerCase();
  let image = [process.env.ImageOS, process.env.ImageVersion].filter(Boolean).join('-');
  if (!image) {
    // Self-hosted runners: fall back to the distribution release.
    const osRelease = fs.existsSync('/etc/os-release') ? fs.readFileSync('/etc/os-release', 'utf8') : '';
    const field = (k) => (osRelease.match(new RegExp(`^${k}="?([^"\\n]*)"?$`, 'm')) || [])[1];
    image = [field('ID'), field('VERSION_ID')].filter(Boolean).join('-') || 'unknown';
  }
  return `${image}-${arch}`;
}

function cacheUrl(spec) {
  if (spec === 'none' || /^(s3|file):\/\//.test(spec)) return spec;
  const dir = path.resolve(spec);
  fs.mkdirSync(dir, { recursive: true });
  return `file://${dir}`;
}

function health(url) {
  return new Promise((resolve) => {
    const req = http.get(`${url}/_health`, { timeout: 1000 }, (res) => {
      res.resume();
      resolve(res.statusCode === 200);
    });
    req.on('error', () => resolve(false));
    req.on('timeout', () => req.destroy());
  });
}

async function startProxy(bin, args, workDir) {
  const portFile = path.join(workDir, 'port');
  const logFile = path.join(workDir, 'proxy.log');
  const log = fs.openSync(logFile, 'a');
  const fullArgs = ['--port-file', portFile, ...args];
  console.log(`[command]${bin} ${fullArgs.join(' ')}`);
  // stdio goes to a file, not the runner's pipes, so the step can finish while it runs.
  const child = spawn(bin, fullArgs, { detached: true, stdio: ['ignore', log, log] });
  fs.closeSync(log);
  child.unref();
  c.saveState('pid', child.pid);
  c.saveState('log_file', logFile);

  const deadline = Date.now() + 10_000;
  while (Date.now() < deadline) {
    if (child.exitCode !== null) break;
    if (fs.existsSync(portFile)) {
      const addr = fs.readFileSync(portFile, 'utf8').trim();
      const port = addr.slice(addr.lastIndexOf(':') + 1);
      const url = `http://127.0.0.1:${port}`;
      if (await health(url)) return { child, url, port, logFile };
    }
    await c.sleep(100);
  }
  c.printLog(logFile);
  throw new Error(
    child.exitCode !== null
      ? `unstuck-proxy exited with code ${child.exitCode}`
      : 'unstuck-proxy did not become healthy within 10s',
  );
}

function aptGet(args, timeoutSec) {
  const sudo = process.getuid && process.getuid() === 0 ? [] : ['sudo'];
  const cmd = [
    'timeout',
    '--signal=INT',
    '--kill-after=10',
    String(timeoutSec),
    ...sudo,
    'env',
    'DEBIAN_FRONTEND=noninteractive',
    'apt-get',
    ...args,
  ];
  console.log(`[command]${cmd.join(' ')}`);
  const r = spawnSync(cmd[0], cmd.slice(1), { stdio: 'inherit' });
  if (r.error) throw r.error;
  if (r.status === 124 || r.status === 137) {
    throw new Error(`apt-get ${args.find((a) => !a.startsWith('-') && !a.includes('='))} timed out after ${timeoutSec}s`);
  }
  if (r.status !== 0) throw new Error(`apt-get exited with code ${r.status}`);
}

async function run() {
  const packages = c.input('packages').split(/\s+/).filter(Boolean);
  const keepRunning = c.boolInput('keep-running', false);
  const update = c.boolInput('update', true);
  const recommends = c.boolInput('install-recommends', false);
  const lockTimeout = c.input('lock-timeout', '120');
  const attemptTimeout = c.input('attempt-timeout', '900');

  const workDir = fs.mkdtempSync(path.join(tempRoot(), 'unstuck-proxy-'));
  const statsFile = path.join(workDir, 'stats.json');
  c.saveState('stats_file', statsFile);
  fs.mkdirSync(path.join(workDir, 'tmp'));

  const bin = await resolveBinary();
  const args = [
    '--listen', c.input('listen', '127.0.0.1:0'),
    '--stats-file', statsFile,
    '--temp-dir', path.join(workDir, 'tmp'),
    '--cache', cacheUrl(c.input('cache', 'none')),
    '--cache-prefix', c.input('cache-key-prefix') || defaultCachePrefix(),
    '--min-rate', c.input('min-rate', '50K'),
    '--stall-timeout', c.input('stall-timeout', '20'),
    '--rounds', c.input('rounds', '2'),
  ];
  const mirrors = c.input('mirrors');
  if (mirrors) args.push('--mirrors', mirrors);

  const proxy = await startProxy(bin, args, workDir);
  console.log(`unstuck-proxy is running at ${proxy.url}`);
  c.setOutput('proxy-port', proxy.port);

  let failure;
  try {
    if (packages.length) {
      const opts = [
        '-o', `Acquire::http::Proxy=${proxy.url}`,
        // apt would otherwise send HTTPS sources to the same proxy via CONNECT.
        '-o', 'Acquire::https::Proxy=DIRECT',
        '-o', `DPkg::Lock::Timeout=${lockTimeout}`,
        '-o', 'Acquire::Retries=3',
      ];
      if (update) aptGet([...opts, 'update'], attemptTimeout);
      const install = [...opts, 'install', '-y'];
      if (!recommends) install.push('--no-install-recommends');
      aptGet([...install, ...packages], attemptTimeout);
    }
  } catch (e) {
    failure = e;
  }

  if (keepRunning) {
    c.setOutput('proxy-url', proxy.url);
  } else {
    await c.stopProxy(proxy.child.pid, 40_000, proxy.child);
    c.saveState('stopped', 'true');
    c.printLog(proxy.logFile);
    c.report(statsFile);
  }
  if (failure) throw failure;
}

run().catch((e) => {
  console.log(`::error::${e.message}`);
  process.exitCode = 1;
});
