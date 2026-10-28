'use strict';
// Stops a proxy left running by main.js (keep-running, or a failed main step)
// after its pending cache writes, then reports its statistics.

const c = require('./common');

async function run() {
  const pid = Number(c.getState('pid'));
  if (!pid || c.getState('stopped') === 'true') return;
  if (c.isAlive(pid)) {
    console.log(`Stopping unstuck-proxy (pid ${pid})`);
    await c.stopProxy(pid, 40_000);
  }
  c.printLog(c.getState('log_file'));
  c.report(c.getState('stats_file'));
}

run().catch((e) => {
  console.log(`::warning::${e.message}`);
});
