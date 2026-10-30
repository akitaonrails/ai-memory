/**
 * Start the real server in the configuration one snapshot variant describes.
 * Shared by `npm run schema:update` and the drift test so a snapshot is
 * always generated the way the test later reproduces it.
 *
 * The disabled-with-existing-jobs variant is a real preserved-data restart:
 * a server with chunking on and a provider whose endpoint refuses connections
 * accepts a job start over a seeded session, stops, and the same data
 * directory is served again with chunking off. Nothing is written to the
 * database except through the server's own tools.
 */

import { AiMemoryClient } from "../../src/client.js";
import type { ProcessingJobStatus } from "../../src/types.js";
import type { SnapshotVariant } from "./coverage-check.js";
import { removeServerRoot, startServer, type ServerHandle } from "./server.js";

/** The job a retained variant's first run created. */
export interface SeededJob {
  readonly scope: { workspace: string; project: string };
  readonly sessionId: string;
  /** Status the start call returned. */
  readonly started: ProcessingJobStatus;
}

/** A running variant server; `job` is set for the existing-jobs variant. */
export interface VariantServer extends ServerHandle {
  readonly job?: SeededJob;
}

const SEED_SCOPE = { workspace: "sdk-variant", project: "app" };

/** Start `variant`'s server; `stop()` removes everything it created. */
export async function startVariantServer(variant: SnapshotVariant): Promise<VariantServer> {
  const chunking = variant.features.includes("chunking");
  if (!variant.existingJobs) return startServer({ chunking });

  const first = await startServer({ chunking: true, unreachableLlm: true, keepData: true });
  let job: SeededJob;
  try {
    const sessionId = await first.seedSession(SEED_SCOPE.workspace, SEED_SCOPE.project, "variant-seed");
    const memory = new AiMemoryClient({
      serverUrl: first.url,
      token: first.rootToken,
      scope: SEED_SCOPE,
    });
    const started = await memory.jobs.start({ sessionId, kind: "consolidate" });
    job = { scope: SEED_SCOPE, sessionId, started };
  } catch (error) {
    await first.stop();
    await removeServerRoot(first.root);
    throw error;
  }
  await first.stop();
  let second: ServerHandle;
  try {
    second = await startServer({ chunking, dataRoot: first.root });
  } catch (error) {
    await removeServerRoot(first.root);
    throw error;
  }
  return {
    ...second,
    job,
    stop: async () => {
      await second.stop();
      await removeServerRoot(first.root);
    },
  };
}
