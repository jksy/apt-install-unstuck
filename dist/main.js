'use strict';
// Starts unstuck-proxy for the rest of the job and makes it apt's proxy for
// Ubuntu archives through Acquire::http::Proxy-Auto-Detect.
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

// Address clients should connect to, from the address the proxy listens on.
function connectAddress(listenAddr) {
  const i = listenAddr.lastIndexOf(':');
  let host = listenAddr.slice(0, i).replace(/^\[|\]$/g, '');
  const port = listenAddr.slice(i + 1);
  if (host === '0.0.0.0' || host === '::') host = '127.0.0.1';
  return { host: host.includes(':') ? `[${host}]` : host, port };
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
      const { host, port } = connectAddress(fs.readFileSync(portFile, 'utf8').trim());
      const url = `http://${host}:${port}`;
      if (await health(url)) return { url, host, port };
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

// The proxy started by an earlier step of this job, if it is still running.
async function runningProxy() {
  try {
    const state = JSON.parse(fs.readFileSync(path.join(c.INSTALL_DIR, 'state.json'), 'utf8'));
    if (state.url && (await health(state.url))) return state;
  } catch (_) {
    // not set up yet
  }
  return null;
}

// Other proxy settings for http (a corporate proxy on a self-hosted runner,
// for example) would be overridden by Proxy-Auto-Detect, so leave them alone.
function existingAptProxyConfig() {
  const r = spawnSync('apt-config', ['dump'], { encoding: 'utf8' });
  if (r.status !== 0) return [];
  return r.stdout.split('\n').filter((l) => /^Acquire::http::(Proxy|ProxyAutoDetect)\b/.test(l));
}

function installFile(workDir, name, contents, mode, dest) {
  const src = path.join(workDir, name);
  fs.writeFileSync(src, contents);
  c.asRoot('install', ['-m', mode, src, dest]);
}

function configureApt(workDir, proxy) {
  const hook = path.join(c.INSTALL_DIR, 'apt-proxy-detect');
  installFile(
    workDir,
    'apt-proxy-detect',
    [
      '#!/bin/sh',
      '# Installed by apt-install-unstuck for Acquire::http::Proxy-Auto-Detect.',
      '# apt treats a failing hook as an error, so fall back to DIRECT on any problem.',
      `bin=${c.INSTALL_DIR}/unstuck-proxy`,
      `[ -x "$bin" ] && "$bin" apt-detect --proxy ${proxy.host}:${proxy.port} "$1" 2>/dev/null || echo DIRECT`,
      '',
    ].join('\n'),
    '0755',
    hook,
  );
  // Written last and removed first, so apt never points at a missing hook.
  installFile(
    workDir,
    'apt.conf',
    [
      '// Installed by apt-install-unstuck; removed at the end of the job.',
      '// Ubuntu archive requests go through unstuck-proxy, everything else DIRECT.',
      `Acquire::http::Proxy-Auto-Detect "${hook}";`,
      '',
    ].join('\n'),
    '0644',
    c.APT_CONF,
  );
  c.saveState('apt_conf', 'true');
}

async function run() {
  const reused = await runningProxy();
  if (reused) {
    console.log(`unstuck-proxy is already running at ${reused.url} (set up by an earlier step); reusing it`);
    console.log('Inputs of this step are ignored.');
    c.setOutput('proxy-url', reused.url);
    c.setOutput('proxy-port', reused.port);
    return;
  }
  if (!fs.existsSync('/etc/apt/apt.conf.d')) {
    throw new Error('apt-install-unstuck needs apt (/etc/apt/apt.conf.d not found)');
  }
  // Left behind by a job whose post step did not run (self-hosted runners).
  if (fs.existsSync(c.APT_CONF)) {
    console.log(`Removing stale ${c.APT_CONF}`);
    c.asRoot('rm', ['-f', c.APT_CONF]);
  }

  const workDir = fs.mkdtempSync(path.join(tempRoot(), 'unstuck-proxy-'));
  const statsFile = path.join(workDir, 'stats.json');
  fs.mkdirSync(path.join(workDir, 'tmp'));

  const built = await resolveBinary();
  const bin = path.join(c.INSTALL_DIR, 'unstuck-proxy');
  c.asRoot('install', ['-d', '-m', '0755', c.INSTALL_DIR]);
  c.asRoot('install', ['-m', '0755', built, bin]);
  c.saveState('owner', 'true');
  c.saveState('stats_file', statsFile);

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
  installFile(workDir, 'state.json', JSON.stringify({ url: proxy.url, port: proxy.port }), '0644',
    path.join(c.INSTALL_DIR, 'state.json'));
  console.log(`unstuck-proxy is running at ${proxy.url}`);
  c.setOutput('proxy-url', proxy.url);
  c.setOutput('proxy-port', proxy.port);

  const existing = existingAptProxyConfig();
  if (existing.length) {
    console.log(
      `::warning::apt already has a proxy configured (${existing.join('; ')}); ` +
        'leaving it in place. Use the proxy-url output to send apt through unstuck-proxy explicitly.',
    );
    return;
  }
  configureApt(workDir, proxy);
  console.log(`apt now sends Ubuntu archive requests through unstuck-proxy (${c.APT_CONF})`);
}

// A failure here must not turn into a new reason for the job to fail: nothing
// that could be left behind makes apt worse off than without the action. The
// apt configuration is written last, and its hook falls back to DIRECT when the
// proxy is not answering; the post step cleans up whatever was installed.
run().catch((e) => {
  console.log(`::error::${e.message}`);
  if (c.input('fail-on-error', 'false') === 'true') {
    process.exitCode = 1;
    return;
  }
  console.log(
    'apt-install-unstuck could not be set up; apt uses its mirrors directly for this job. ' +
      'Set the fail-on-error input to true to fail the step instead.',
  );
});
