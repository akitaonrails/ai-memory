// Exercises the generated extension, not a separate implementation of accounting.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { pathToFileURL } from "node:url";

const [extensionPath, scenarioPath] = process.argv.slice(2);
const scenario = JSON.parse(readFileSync(scenarioPath!, "utf8"));
const { default: extension, drainHookQueueForDispose } = await import(pathToFileURL(extensionPath!).href);
const hooks: any[] = [];
const nativeFetch = globalThis.fetch;
globalThis.fetch = async (url: any, options: any) => {
  if (new URL(url).pathname === "/hook") {
    hooks.push({ event: new URL(url).searchParams.get("event"), body: JSON.parse(options.body) });
  }
  return nativeFetch(url, options);
};
const handlers: Record<string, any> = {};
extension({ on: (name: string, handler: any) => { handlers[name] = handler; }, registerTool: () => {} });
for (const step of scenario.steps) {
  const ctx = {
    cwd: step.cwd ?? scenario.cwd,
    model: { id: "pi-fixture-model" },
    sessionManager: {
      getSessionId: () => step.session,
      getEntries: () => step.entries,
      // Accounting must not use the compacted/active context instead of raw history.
      getBranch: () => [],
    },
  };
  await handlers[step.event]?.(step.payload ?? {}, ctx);
  await drainHookQueueForDispose();
  if (step.expected !== undefined) {
    const hook = hooks.filter((h) => h.body.sessionID === step.session && h.event === step.hook).at(-1);
    assert.ok(hook, `missing ${step.hook} hook for ${step.session}`);
    assert.deepEqual(hook.body._ai_memory_usage ?? null, step.expected);
  }
}
console.log(JSON.stringify(hooks));
// The fixture owns no production resources. MCP bootstrap/flush timers need not keep Node alive.
process.exit(0);
