#!/usr/bin/env node
// Create (and optionally inspect) a Composio trigger instance via the SDK.
// Org-free path for personal accounts — needs only COMPOSIO_API_KEY, no
// `composio dev` / developer project.
//
// Setup:
//   npm i @composio/core
//   export COMPOSIO_API_KEY=sk_...
//
// Inspect a trigger type's required config:
//   node scripts/trigger_create.mjs --slug GOOGLESHEETS_CELL_RANGE_VALUES_CHANGED --show
//
// List your connected accounts (to find toolkit/status):
//   node scripts/trigger_create.mjs --list
//
// Create a Google Sheets cell-range trigger on the psflow-test sheet:
//   node scripts/trigger_create.mjs \
//     --user-id default \
//     --slug GOOGLESHEETS_CELL_RANGE_VALUES_CHANGED \
//     --sheet 1b2h56BL2C8kKM8WGPt_WjonN3bLmMwaRBM-poILGz7A \
//     --range "Sheet1!A1:C20"
//
// Prints the trigger instance id (ti_…) to wire into `just triggers <handler> <ti_id>`.

import { Composio } from '@composio/core';

function arg(name, fallback) {
  const i = process.argv.indexOf(`--${name}`);
  if (i !== -1 && process.argv[i + 1] && !process.argv[i + 1].startsWith('--')) {
    return process.argv[i + 1];
  }
  return fallback;
}
const has = (name) => process.argv.includes(`--${name}`);

async function main() {
  const composio = new Composio(); // reads COMPOSIO_API_KEY

  if (has('list')) {
    const accounts = await composio.connectedAccounts.list();
    const items = accounts.items ?? accounts;
    for (const a of items) {
      const tk = a.toolkit?.slug ?? a.toolkit;
      console.log(`${a.id}  ${tk}  ${a.status}`);
    }
    return;
  }

  const slug = arg('slug', 'GOOGLESHEETS_CELL_RANGE_VALUES_CHANGED');

  // Show the trigger type's required config fields (always — it's the SSOT for
  // the field names below).
  const type = await composio.triggers.getType(slug);
  console.error(`[trigger_create] config schema for ${slug}:`);
  console.error(JSON.stringify(type.config, null, 2));
  if (has('show')) return;

  const userId = arg('user-id');
  if (!userId) {
    console.error('error: --user-id is required to create (the user the connected account is under)');
    process.exit(1);
  }

  // Build triggerConfig: explicit --config JSON wins; otherwise convenience
  // --sheet/--range for the common Google Sheets case.
  let triggerConfig;
  const rawConfig = arg('config');
  if (rawConfig) {
    triggerConfig = JSON.parse(rawConfig);
  } else {
    triggerConfig = {};
    const sheet = arg('sheet');
    const range = arg('range');
    if (sheet) triggerConfig.spreadsheet_id = sheet;
    if (range) triggerConfig.range = range;
  }

  const trigger = await composio.triggers.create(userId, slug, { triggerConfig });
  console.error('[trigger_create] created.');
  // triggerId to stdout so it can be captured into a var.
  console.log(trigger.triggerId);
}

main().catch((err) => {
  console.error(`[trigger_create] error: ${err?.message ?? err}`);
  process.exit(1);
});
