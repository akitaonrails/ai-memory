/**
 * `npm run schema:check`: the static half of the drift check. Without a
 * server, confirm that the coverage table and every committed snapshot agree
 * (one snapshot per variant, see test/support/coverage-check.ts): every
 * snapshot tool is classified, every tool a variant must list is in its
 * snapshot, a feature-gated tool is absent from variants that do not enable
 * it (the read-only job status excepted where earlier jobs exist), and every
 * wire argument a wrapper can send exists in the tool's input schema. A typo
 * in a wire name fails here.
 */

import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";

import {
  crossVariantProblems,
  DEFAULT_VARIANT,
  ENABLED_VARIANT,
  RETAINED_VARIANT,
  retainedToolProblems,
  UPDATE_HINT,
  variantProblems,
  VARIANTS,
} from "../test/support/coverage-check.js";
import type { NormalizedTools } from "../test/support/schema.js";

const problems: string[] = [];
const snapshots = new Map<string, NormalizedTools>();

for (const variant of VARIANTS) {
  const path = fileURLToPath(new URL(`../../schema/${variant.file}`, import.meta.url));
  let text: string;
  try {
    text = await readFile(path, "utf8");
  } catch {
    problems.push(`schema/${variant.file} (${variant.name}) is missing`);
    continue;
  }
  const snapshot = JSON.parse(text) as NormalizedTools;
  snapshots.set(variant.name, snapshot);
  problems.push(...variantProblems(variant, snapshot));
}
const base = snapshots.get(DEFAULT_VARIANT.name);
const enabled = snapshots.get(ENABLED_VARIANT.name);
const retained = snapshots.get(RETAINED_VARIANT.name);
if (base !== undefined && enabled !== undefined) {
  problems.push(...crossVariantProblems(base, enabled, ENABLED_VARIANT.name));
}
if (base !== undefined && retained !== undefined) {
  problems.push(...crossVariantProblems(base, retained, RETAINED_VARIANT.name));
}
if (enabled !== undefined && retained !== undefined) {
  problems.push(...retainedToolProblems(enabled, retained));
}

if (problems.length > 0) {
  console.error(problems.join("\n"));
  console.error(`\nFix the wire name, or: ${UPDATE_HINT}`);
  process.exit(1);
}
console.log(
  `schema check passed: ${[...snapshots].map(([name, s]) => `${name} ${Object.keys(s).length}`).join(", ")} tools classified`,
);
