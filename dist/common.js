'use strict';
// Shared helpers for main.js and post.js. Plain Node.js, no dependencies.

const fs = require('fs');
const { spawnSync } = require('child_process');

function input(name, fallback = '') {
  const v = process.env[`INPUT_${name.replace(/ /g, '_').toUpperCase()}`];
  return v === undefined || v.trim() === '' ? fallback : v.trim();
}

function appendCommandFile(envName, text) {
  const file = process.env[envName];
  if (!file) throw new Error(`${envName} is not set`);
  fs.appendFileSync(file, text);
}

function keyValue(name, value) {
  const s = String(value);
  if (!s.includes('\n')) return `${name}=${s}\n`;
  const delim = `EOF_${Math.random().toString(36).slice(2)}`;
  return `${name}<<${delim}\n${s}\n${delim}\n`;
}

const setOutput = (name, value) => appendCommandFile('GITHUB_OUTPUT', keyValue(name, value));
const saveState = (name, value) => appendCommandFile('GITHUB_STATE', keyValue(name, value));
const getState = (name) => process.env[`STATE_${name}`] || '';

function isAlive(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch (e) {
    return e.code === 'EPERM';
  }
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// Runs a command as root (directly when already root, e.g. in a container job).
function asRoot(cmd, args) {
  const isRoot = process.getuid && process.getuid() === 0;
  const [file, argv] = isRoot ? [cmd, args] : ['sudo', ['-n', cmd, ...args]];
  const r = spawnSync(file, argv, { stdio: ['ignore', 'inherit', 'inherit'] });
  if (r.error) throw new Error(`cannot run ${file}: ${r.error.message}`);
  if (r.status !== 0) throw new Error(`${[file, ...argv].join(' ')} exited with code ${r.status}`);
}

// Sends SIGTERM and waits for the process to exit; SIGKILL after the timeout.
async function stopProxy(pid, timeoutMs) {
  if (!isAlive(pid)) return;
  process.kill(pid, 'SIGTERM');
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (!isAlive(pid)) return;
    await sleep(200);
  }
  console.log(`::warning::unstuck-proxy did not stop within ${timeoutMs / 1000}s; killing it`);
  try {
    process.kill(pid, 'SIGKILL');
  } catch (_) {
    // already gone
  }
}

function printLog(logFile) {
  if (!logFile || !fs.existsSync(logFile)) return;
  console.log('::group::unstuck-proxy log');
  console.log(fs.readFileSync(logFile, 'utf8'));
  console.log('::endgroup::');
}

function fmtBytes(n) {
  const units = ['B', 'KiB', 'MiB', 'GiB'];
  let i = 0;
  while (n >= 1024 && i < units.length - 1) {
    n /= 1024;
    i++;
  }
  return `${i === 0 ? n : n.toFixed(1)} ${units[i]}`;
}

// Reads the stats file and writes the step summary.
function report(statsFile) {
  if (!statsFile || !fs.existsSync(statsFile)) {
    console.log('::warning::unstuck-proxy did not write statistics');
    return null;
  }
  const s = JSON.parse(fs.readFileSync(statsFile, 'utf8'));
  console.log(
    `unstuck-proxy: hits=${s.hits} misses=${s.misses} stale=${s.stale} passthrough=${s.passthrough} ` +
      `errors=${s.errors} failovers=${s.failovers}`,
  );
  const rows = [
    ['Cache hits', s.hits],
    ['Cache misses', s.misses],
    ['Stale (served from cache after upstream errors)', s.stale],
    ['Passthrough', s.passthrough],
    ['Errors', s.errors],
    ['Mirror failovers', s.failovers],
    ['From cache', fmtBytes(s.bytes_from_cache)],
    ['From upstream', fmtBytes(s.bytes_from_upstream)],
    ['Cache writes', `${s.cache_writes} (${s.cache_write_errors} failed)`],
  ];
  let md = '### apt-install-unstuck\n\n| Metric | Value |\n|---|---:|\n';
  md += rows.map(([k, v]) => `| ${k} | ${v} |`).join('\n') + '\n';
  const mirrors = Object.entries(s.mirrors || {});
  if (mirrors.length) {
    md += '\n| Mirror | Success | Failure |\n|---|---:|---:|\n';
    md += mirrors.map(([m, v]) => `| ${m} | ${v.success} | ${v.failure} |`).join('\n') + '\n';
  }
  if (process.env.GITHUB_STEP_SUMMARY) appendCommandFile('GITHUB_STEP_SUMMARY', md + '\n');
  return s;
}

// Files installed outside RUNNER_TEMP: apt runs the detect hook as the _apt
// user, which cannot read the runner's home directory.
const INSTALL_DIR = '/usr/local/lib/apt-install-unstuck';
const APT_CONF = '/etc/apt/apt.conf.d/99apt-install-unstuck';

module.exports = {
  INSTALL_DIR,
  APT_CONF,
  input,
  setOutput,
  saveState,
  getState,
  isAlive,
  sleep,
  asRoot,
  stopProxy,
  printLog,
  report,
};
