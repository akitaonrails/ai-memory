/**
 * Schema drift (design doc §5.3): each committed snapshot must match the
 * `tools/list` of the server built from the same commit, run once per
 * variant (fresh flag off, chunking on, chunking off over existing jobs), and
 * every server tool must be classified in `src/coverage.ts`. A server change
 * to an MCP tool fails here until the SDK is updated in the same change.
 */

import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { after, before, describe, it } from "node:test";
import { fileURLToPath } from "node:url";

import { AiMemoryClient } from "../../src/client.js";
import { AiMemoryError, UnsupportedServerError } from "../../src/errors.js";
import {
  crossVariantProblems,
  DEFAULT_VARIANT,
  ENABLED_VARIANT,
  RETAINED_VARIANT,
  retainedToolProblems,
  UPDATE_HINT,
  variantProblems,
  VARIANTS,
  type SnapshotVariant,
} from "../support/coverage-check.js";
import { diffTools, normalizeTools, type NormalizedTools } from "../support/schema.js";
import { startVariantServer, type VariantServer } from "../support/variant-server.js";
import { fetchToolsList } from "../support/tools-list.js";

interface Loaded {
  server: VariantServer;
  live: NormalizedTools;
  snapshot: NormalizedTools;
}

const loaded = new Map<string, Loaded>();
const UNKNOWN_JOB = "00000000-0000-0000-0000-000000000000";

before(async () => {
  for (const variant of VARIANTS) {
    const server = await startVariantServer(variant);
    const live = normalizeTools(await fetchToolsList(server.url, server.rootToken));
    const path = fileURLToPath(new URL(`../../../schema/${variant.file}`, import.meta.url));
    const snapshot = JSON.parse(await readFile(path, "utf8")) as NormalizedTools;
    loaded.set(variant.name, { server, live, snapshot });
  }
});
after(async () => {
  for (const { server } of loaded.values()) await server.stop();
});

function entry(variant: SnapshotVariant): Loaded {
  const found = loaded.get(variant.name);
  assert.ok(found !== undefined, `${variant.name} server started`);
  return found;
}

function client(variant: SnapshotVariant, scope = { workspace: "it-drift", project: "app" }) {
  const { server } = entry(variant);
  return new AiMemoryClient({ serverUrl: server.url, token: server.rootToken, scope });
}

for (const variant of VARIANTS) {
  describe(`schema drift: ${variant.name} (features: ${variant.features.join(", ") || "none"}${variant.existingJobs ? ", existing jobs" : ""})`, () => {
    it(`schema/${variant.file} matches the server's tools/list`, () => {
      const { live, snapshot } = entry(variant);
      const findings = diffTools(snapshot, live);
      assert.deepEqual(findings, [], `${findings.join("\n")}\n\n${UPDATE_HINT}`);
    });

    it("every listed tool is classified and exactly the expected set is listed", () => {
      const { live } = entry(variant);
      const problems = variantProblems(variant, live);
      assert.deepEqual(problems, [], `${problems.join("\n")}\n\n${UPDATE_HINT}`);
    });
  });
}

describe("schema drift: variants relate to each other", () => {
  it("the variants list 25, 27 and 26 tools", () => {
    assert.equal(Object.keys(entry(DEFAULT_VARIANT).live).length, 25);
    assert.equal(Object.keys(entry(ENABLED_VARIANT).live).length, 27);
    assert.equal(Object.keys(entry(RETAINED_VARIANT).live).length, 26);
  });

  it("neither variant reshapes a tool the fresh flag-off server lists", () => {
    const base = entry(DEFAULT_VARIANT).live;
    assert.deepEqual(crossVariantProblems(base, entry(ENABLED_VARIANT).live, "chunking"), []);
    assert.deepEqual(crossVariantProblems(base, entry(RETAINED_VARIANT).live, "retained"), []);
  });

  it("the retained job status has the schema it has when chunking is on", () => {
    assert.deepEqual(
      retainedToolProblems(entry(ENABLED_VARIANT).live, entry(RETAINED_VARIANT).live),
      [],
    );
  });

  it("the diff names the tool and the property that changed", () => {
    const { live } = entry(DEFAULT_VARIANT);
    const changed: NormalizedTools = structuredClone(live);
    const write = changed["memory_write_page"];
    assert.ok(write !== undefined);
    delete write.properties["expected_page_id"];
    changed["memory_new_tool"] = { properties: {}, required: [] };
    delete changed["memory_status"];
    assert.deepEqual(diffTools(live, changed), [
      "new tool on the server: memory_new_tool",
      "tool removed from the server: memory_status",
      "memory_write_page: properties differ: expected_page_id",
    ]);
  });

  it("an unclassified extra tool or a leaked gated tool fails the variant check", () => {
    const { live } = entry(DEFAULT_VARIANT);
    const extra: NormalizedTools = {
      ...live,
      memory_new_tool: { properties: {}, required: [] },
      memory_processing_job_start: { properties: {}, required: [] },
    };
    const problems = variantProblems(DEFAULT_VARIANT, extra);
    assert.ok(problems.some((p) => p.includes("memory_new_tool is listed but not classified")));
    assert.ok(problems.some((p) => p.includes("memory_processing_job_start needs the chunking")));
  });

  it("a start tool leaking into the existing-jobs variant fails, the status tool does not", () => {
    const start = entry(ENABLED_VARIANT).live["memory_processing_job_start"];
    assert.ok(start !== undefined);
    const leaked: NormalizedTools = {
      ...entry(RETAINED_VARIANT).live,
      memory_processing_job_start: structuredClone(start),
    };
    const problems = variantProblems(RETAINED_VARIANT, leaked);
    assert.equal(problems.length, 1);
    assert.match(problems[0] ?? "", /memory_processing_job_start needs the chunking feature/);
  });
});

describe("processing jobs against a real server", () => {
  it("fresh flag off: tools not listed, wrappers refuse before sending, direct start is refused", async () => {
    const memory = client(DEFAULT_VARIANT);
    assert.deepEqual(await memory.jobs.discover(), { start: false, status: false });
    await assert.rejects(
      memory.jobs.start({ sessionId: UNKNOWN_JOB, kind: "consolidate" }),
      UnsupportedServerError,
    );
    await assert.rejects(memory.jobs.status({ jobId: UNKNOWN_JOB }), UnsupportedServerError);
    // A raw call skips the tool list; the server itself must refuse it explicitly.
    await assert.rejects(
      memory.callTool("memory_processing_job_start", {
        workspace: "it-drift",
        project: "app",
        session_id: UNKNOWN_JOB,
        kind: "consolidate",
      }),
      AiMemoryError,
    );
  });

  it("chunking on: both tools listed and an unknown job id is refused", async () => {
    const memory = client(ENABLED_VARIANT);
    assert.deepEqual(await memory.jobs.discover(), { start: true, status: true });
    await assert.rejects(memory.jobs.status({ jobId: UNKNOWN_JOB }), AiMemoryError);
  });

  it("chunking off over existing jobs: status only, and it decodes the job the enabled run created", async () => {
    const { server } = entry(RETAINED_VARIANT);
    const job = server.job;
    assert.ok(job !== undefined, "the existing-jobs variant seeds a job");
    const memory = client(RETAINED_VARIANT, job.scope);
    assert.deepEqual(await memory.jobs.discover(), { start: false, status: true });

    const status = await memory.jobs.status({ jobId: job.started.jobId });
    assert.equal(status.jobId, job.started.jobId);
    assert.equal(status.sessionId, job.started.sessionId);
    assert.equal(status.kind, "consolidate");
    assert.equal(typeof status.state, "string");
    assert.ok(Number.isInteger(status.chunksTotal) && status.chunksTotal >= 0);
    assert.ok(Number.isInteger(status.chunksDone) && status.chunksDone >= 0);
    assert.ok(Number.isInteger(status.callsUsed) && status.callsUsed >= 0);
    assert.ok(Number.isInteger(status.waitMs) && status.waitMs >= 0);
    assert.ok(Number.isInteger(status.execMs) && status.execMs >= 0);
    assert.ok(status.maxCalls > 0);

    // The server shows the same job to a raw caller; a foreign project cannot read it.
    const raw = await memory.callTool("memory_processing_job_status", {
      workspace: job.scope.workspace,
      project: job.scope.project,
      job_id: job.started.jobId,
    });
    assert.ok(raw !== null && typeof raw === "object" && "job_id" in raw);
    assert.equal(raw.job_id, job.started.jobId);
    await assert.rejects(
      memory.jobs.status({
        jobId: job.started.jobId,
        scope: { workspace: job.scope.workspace, project: "other" },
      }),
      AiMemoryError,
    );

    // Start is not offered, and direct dispatch is refused: never a silent sync fallback.
    await assert.rejects(
      memory.jobs.start({ sessionId: job.sessionId, kind: "consolidate" }),
      UnsupportedServerError,
    );
    await assert.rejects(
      memory.callTool("memory_processing_job_start", {
        workspace: job.scope.workspace,
        project: job.scope.project,
        session_id: job.sessionId,
        kind: "consolidate",
      }),
      AiMemoryError,
    );
  });
});
