# Composio Triggers — SDK reference (org-free path)

The org-scoped CLI path (`composio dev init` → `dev triggers` → `dev listen`)
requires a developer project, which a personal workspace doesn't have (it fails
with `Failed to list org projects: HTTP 404`). The SDK path below needs only
`COMPOSIO_API_KEY` — no dev project, no org.

## 1. Canonical snippet (from the Composio dashboard)

```typescript
import { Composio } from "@composio/core";

const composio = new Composio();

// Check what configuration is required for a trigger
const triggerType = await composio.triggers.getType("GITHUB_COMMIT_EVENT");
console.log(triggerType.config);

// Create a trigger with the required config
const trigger = await composio.triggers.create(
  "<your-user-id>",
  "GITHUB_COMMIT_EVENT",
  {
    triggerConfig: {
      owner: "your-repo-owner",
      repo: "your-repo-name",
    },
  },
);
console.log(`Trigger created: ${trigger.triggerId}`);

// Subscribe to trigger events
await composio.triggers.subscribe(
  (data) => {
    console.log("Event received:", data);
  },
  { triggerId: trigger.triggerId },
);
```

## 2. Confirmed API signatures (`@composio/core`)

- `composio.triggers.getType(slug)` → `{ config, ... }` — `config` is the SSOT for the trigger's required `triggerConfig` field names.
- `composio.triggers.create(userId, slug, { triggerConfig })` → `{ triggerId }`. Note the argument order: **userId first, then slug**, then options. The connected account is resolved from `userId` + the slug's toolkit.
- `composio.triggers.subscribe(callback, filter)` — `filter` keys: `triggerId`, `triggerSlug`, `toolkits`, `connectedAccountId`, `authConfigId`, `userId`, `triggerData`. `subscribe` resolves once subscribed (websocket stays open) — keep the process alive.
- `composio.connectedAccounts.list()` → `{ items: [{ id, toolkit, status, ... }] }` (no `user_id` on the item).

## 3. Helper scripts in this repo

- `scripts/trigger_create.mjs` — wraps `getType` + `create` (and `--list` connected accounts). Prints the `ti_…` id.
- `scripts/triggers_listen.mjs` — wraps `subscribe`, emitting each event as a JSON line for `psflow-run --listen` (via `PSFLOW_LISTEN_CMD`). Run end-to-end with `just triggers <handler> <ti_id>`.

## 4. End-to-end (Google Sheets example)

```bash
npm i @composio/core
export COMPOSIO_API_KEY=sk_...

# inspect required config + create the trigger on the psflow-test sheet
node scripts/trigger_create.mjs \
  --user-id default \
  --slug GOOGLESHEETS_CELL_RANGE_VALUES_CHANGED \
  --sheet 1b2h56BL2C8kKM8WGPt_WjonN3bLmMwaRBM-poILGz7A \
  --range "Sheet1!A1:C20"          # prints ti_…

# listen → run sheet-summary on every change
just triggers sheet-summary ti_xxx
```

Trade-off: this receive path is **not keyless** (the SDK needs the API key),
unlike the CLI tool-execution path which uses `composio login`.
