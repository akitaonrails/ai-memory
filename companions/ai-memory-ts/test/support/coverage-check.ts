/**
 * Coverage validation shared by `npm run schema:check` and the drift test.
 * The server lists a different tool set per feature flag and per stored data,
 * so there is one committed snapshot per variant, and each is checked against
 * `src/coverage.ts`: every required tool is present, nothing unclassified is
 * present, and a feature-gated tool is present only in a variant that
 * enables its feature (or, for a read-only `retainedAfterDisable` tool, keeps
 * data the feature created).
 */

import { expectedTools, TOOL_COVERAGE, type ToolFeature } from "../../src/coverage.js";
import { diffTools, type NormalizedTools } from "./schema.js";

/** One server configuration whose `tools/list` is committed as a snapshot. */
export interface SnapshotVariant {
  /** Name used in messages. */
  readonly name: string;
  /** File under `schema/`. */
  readonly file: string;
  /** Features on in this variant. */
  readonly features: readonly ToolFeature[];
  /**
   * The data directory was written by an earlier run with `chunking` on, then
   * the server restarted with the flag off, so the read-only job tool stays
   * listed.
   */
  readonly existingJobs: boolean;
}

/** Fresh flag-off default (the legacy 25 tools), chunking on, and flag off over existing jobs. */
export const VARIANTS: readonly SnapshotVariant[] = [
  { name: "default", file: "tools.json", features: [], existingJobs: false },
  { name: "chunking", file: "tools.chunking.json", features: ["chunking"], existingJobs: false },
  {
    name: "chunking-off-existing-jobs",
    file: "tools.chunking-retained.json",
    features: [],
    existingJobs: true,
  },
];

/** The fresh flag-off variant. */
export const DEFAULT_VARIANT: SnapshotVariant = VARIANTS[0] as SnapshotVariant;
/** The variant with chunking enabled. */
export const ENABLED_VARIANT: SnapshotVariant = VARIANTS[1] as SnapshotVariant;
/** The variant with chunking disabled over a data directory that holds jobs. */
export const RETAINED_VARIANT: SnapshotVariant = VARIANTS[2] as SnapshotVariant;

/** The command that regenerates every snapshot. */
export const UPDATE_HINT =
  "Run `AI_MEMORY_BIN=<built ai-memory> npm run schema:update` in companions/ai-memory-ts " +
  "(it starts one server per variant, creating a job in a preserved data directory for the " +
  "disabled-with-existing-jobs variant), then update src/coverage.ts and the wrapper, its types, " +
  "tests and docs.";

/** Problems of one snapshot against the coverage table; empty when consistent. */
export function variantProblems(variant: SnapshotVariant, snapshot: NormalizedTools): string[] {
  const problems: string[] = [];
  const where = `schema/${variant.file} (${variant.name})`;
  const required = new Set(expectedTools(variant.features, variant.existingJobs));
  for (const tool of Object.keys(snapshot)) {
    const entry = TOOL_COVERAGE[tool];
    if (entry === undefined) {
      problems.push(`${where}: ${tool} is listed but not classified in src/coverage.ts`);
    } else if (!required.has(tool)) {
      problems.push(
        `${where}: ${tool} needs the ${entry.feature ?? "?"} feature but is listed without it`,
      );
    }
  }
  for (const tool of required) {
    const schema = snapshot[tool];
    if (schema === undefined) {
      problems.push(`${where}: ${tool} is classified in src/coverage.ts but missing`);
      continue;
    }
    const entry = TOOL_COVERAGE[tool];
    if (entry?.status !== "wrapped") continue;
    for (const arg of entry.sends) {
      if (!(arg in schema.properties)) {
        problems.push(`${where}: ${tool}: the SDK can send "${arg}", which the schema lacks`);
      }
    }
  }
  return problems;
}

/**
 * A feature variant may add tools but must not reshape the tools the fresh
 * flag-off server lists: legacy clients keep working against it.
 */
export function crossVariantProblems(
  base: NormalizedTools,
  other: NormalizedTools,
  label: string,
): string[] {
  const gated = Object.entries(TOOL_COVERAGE)
    .filter(([, entry]) => entry.feature !== undefined)
    .map(([name]) => name);
  const rest = { ...other };
  for (const name of gated) delete rest[name];
  return diffTools(base, rest).map((finding) => `${label} snapshot vs default: ${finding}`);
}

/**
 * A gated tool the retained (flag off, existing jobs) variant keeps listing
 * must have exactly the schema the enabled variant gives it.
 */
export function retainedToolProblems(enabled: NormalizedTools, retained: NormalizedTools): string[] {
  const pick = (snapshot: NormalizedTools): NormalizedTools => {
    const out: NormalizedTools = {};
    for (const name of Object.keys(retained)) {
      const tool = snapshot[name];
      const entry = TOOL_COVERAGE[name];
      if (tool !== undefined && entry?.retainedAfterDisable === true) out[name] = tool;
    }
    return out;
  };
  return diffTools(pick(enabled), pick(retained)).map(
    (finding) => `retained snapshot vs chunking: ${finding}`,
  );
}
