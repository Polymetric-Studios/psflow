#!/usr/bin/env node
// Emit Composio trigger events as JSON lines for `psflow-run --listen`.
//
// Receives events via the SDK websocket (`composio.triggers.subscribe`), which
// needs only COMPOSIO_API_KEY — no `composio dev` / org / developer project.
// This is the receive path for personal accounts where `composio dev listen`
// returns "Failed to list org projects: HTTP 404".
//
// Each event is printed as one compact JSON object per line on stdout (the
// format `psflow-run --listen` consumes via PSFLOW_LISTEN_CMD).
//
// Setup:
//   npm i @composio/core
//   export COMPOSIO_API_KEY=sk_...
//
// Standalone:
//   node scripts/triggers_listen.mjs [--trigger-id ti_xxx]
//
// Wired into the bridge (each event runs the handler graph with {ctx.event}):
//   PSFLOW_LISTEN_CMD='node scripts/triggers_listen.mjs --trigger-id ti_xxx' \
//     psflow-run --listen on-event

import { Composio } from '@composio/core';

function emit(data) {
  let line;
  try {
    line = JSON.stringify(data);
  } catch {
    line = JSON.stringify({ raw: String(data) });
  }
  process.stdout.write(line + '\n');
}

function parseTriggerId(argv) {
  const i = argv.indexOf('--trigger-id');
  return i !== -1 && argv[i + 1] ? argv[i + 1] : undefined;
}

async function main() {
  const triggerId = parseTriggerId(process.argv.slice(2));
  const composio = new Composio(); // reads COMPOSIO_API_KEY from the environment
  process.stderr.write('[triggers_listen] subscribing…\n');

  // subscribe(callback, filter?) — filter by trigger id when provided, else
  // receive all of the account's trigger events.
  await composio.triggers.subscribe(emit, triggerId ? { triggerId } : {});
  process.stderr.write('[triggers_listen] subscribed; waiting for events…\n');
}

main().catch((err) => {
  process.stderr.write(`[triggers_listen] error: ${err?.message ?? err}\n`);
  process.exit(1);
});
