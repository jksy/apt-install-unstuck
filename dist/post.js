'use strict';
// Undoes main.js: removes the apt configuration, stops the proxy after its
// pending cache writes, and reports its statistics. Steps that reused a proxy
// started earlier in the job leave the cleanup to that step.

const c = require('./common');

function tryAsRoot(cmd, args) {
  try {
    c.asRoot(cmd, args);
  } catch (e) {
    console.log(`::warning::${e.message}`);
  }
}

async function run() {
  if (c.getState('owner') !== 'true') return;
  // The apt configuration goes first so apt never points at a missing hook.
  if (c.getState('apt_conf') === 'true') tryAsRoot('rm', ['-f', c.APT_CONF]);
  const pid = Number(c.getState('pid'));
  if (pid && c.isAlive(pid)) {
    console.log(`Stopping unstuck-proxy (pid ${pid})`);
    await c.stopProxy(pid, 40_000);
  }
  tryAsRoot('rm', ['-rf', c.INSTALL_DIR]);
  c.printLog(c.getState('log_file'));
  c.report(c.getState('stats_file'));
}

run().catch((e) => {
  console.log(`::warning::${e.message}`);
});
