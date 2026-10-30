/**
 * `npm run schema:update`: start the server named by AI_MEMORY_BIN once per
 * snapshot variant, read each `tools/list`, and rewrite the snapshots under
 * `schema/`: `tools.json` (fresh data directory, `[chunking]` off),
 * `tools.chunking.json` (`[chunking] enabled = true`) and
 * `tools.chunking-retained.json` (chunking off over a preserved data
 * directory in which an enabled run created a job). Run it after a server
 * change to an MCP tool, then update the wrapper, its types, tests and docs.
 */

import { writeFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";

import { VARIANTS } from "../test/support/coverage-check.js";
import { fetchToolsList } from "../test/support/tools-list.js";
import { normalizeTools, stableStringify } from "../test/support/schema.js";
import { startVariantServer } from "../test/support/variant-server.js";

for (const variant of VARIANTS) {
  const target = fileURLToPath(new URL(`../../schema/${variant.file}`, import.meta.url));
  const server = await startVariantServer(variant);
  try {
    const normalized = normalizeTools(await fetchToolsList(server.url, server.rootToken));
    await writeFile(target, stableStringify(normalized));
    console.log(`wrote ${Object.keys(normalized).length} tools to schema/${variant.file}`);
  } finally {
    await server.stop();
  }
}
