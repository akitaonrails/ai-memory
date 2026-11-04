// Execute the generated adapter, rather than only checking template substrings.
// Run by `cargo test` (opencode2_plugin_passes_the_node_host_fixture), or by hand:
// AI_MEMORY_DATA_DIR=<empty dir> node --experimental-strip-types opencode2-plugin.mjs /absolute/plugin.ts
import assert from "node:assert/strict";
import { pathToFileURL } from "node:url";
import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";

const requests = [];
globalThis.fetch = async (input, options = {}) => {
  // Like a real fetch, a request cancelled before it is sent never arrives.
  options.signal?.throwIfAborted();
  const url = new URL(input);
  requests.push({ url, payload: options.body ? JSON.parse(options.body) : undefined });
  // A server slower than the plugin's timeout: the fetch rejects like an abort.
  if (url.pathname === "/handoff" && url.searchParams.get("session_id") === "root-slow") {
    throw new DOMException("The operation timed out.", "TimeoutError");
  }
  if (url.pathname === "/recap" && url.searchParams.get("mode") === "anchor") {
    return new Response(`ANCHOR ${url.searchParams.get("session_id")}`);
  }
  return new Response(url.pathname === "/handoff" ? "Remember the verified handoff." : "{}");
};
const { default: plugin } = await import(pathToFileURL(process.argv[2]).href);

function host(directory, records) {
  const hooks = new Map();
  const events = [];
  let wake;
  return {
    hooks,
    emit(event) {
      events.push(event);
      wake?.();
    },
    ctx: {
      location: { directory },
      session: {
        async get({ sessionID }) {
          const record = records.get(sessionID);
          assert.ok(record, `unknown session ${sessionID}`);
          return record;
        },
        async hook(name, callback) {
          hooks.set(`session.${name}`, callback);
          return { async dispose() {} };
        },
      },
      tool: {
        async hook(name, callback) {
          hooks.set(`tool.${name}`, callback);
          return { async dispose() {} };
        },
      },
      event: {
        async *subscribe({ signal }) {
          signal.addEventListener("abort", () => wake?.(), { once: true });
          while (!signal.aborted) {
            if (!events.length) await new Promise((resolve) => { wake = resolve; });
            while (events.length) yield events.shift();
          }
        },
      },
    },
  };
}

async function until(predicate, message) {
  const deadline = performance.now() + 5000;
  while (!predicate()) {
    assert.ok(performance.now() < deadline, message);
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
}

const root = process.cwd();
const info = (id, parentID) => ({ id, title: id, location: { directory: root }, parentID });
const records = new Map([["root-a", info("root-a")], ["root-b", { ...info("root-b"), location: { directory: root + "/beta" } }],
  ["child", info("child", "root-a")], ["root-slow", info("root-slow")]]);
const a = host(root, records);
const b = host(root + "/beta", records);
const disposeA = await plugin.setup(a.ctx);
const disposeB = await plugin.setup(b.ctx);
try {
  // Existing/resumed sessions may never emit session.created after plugin load.
  await a.hooks.get("session.prompt")({ sessionID: "root-a", messageID: "m1", prompt: { text: "remember alpha" } });
  const first = { sessionID: "root-a", system: [] };
  const next = { sessionID: "root-a", system: [] };
  await a.hooks.get("session.context")(first);
  await a.hooks.get("session.context")(next);
  assert.deepEqual(first.system, next.system);
  assert.equal(first.system.length, 1);
  assert.equal(requests.filter((r) => r.url.pathname === "/handoff").length, 1, "claim once, inject repeatedly");

  await a.hooks.get("session.prompt")({ sessionID: "child", messageID: "c1", prompt: { text: "child work" } });
  const child = { sessionID: "child", system: [] };
  await a.hooks.get("session.context")(child);
  assert.equal(child.system.length, 0);
  assert.equal(requests.filter((r) => r.url.pathname === "/handoff").length, 1, "child must not claim");

  // A failed startup-context fetch is retried on a later model request, but a
  // bounded number of times: the context hook fires on every model call.
  const slowHandoffs = () => requests.filter((r) => r.url.pathname === "/handoff" && r.url.searchParams.get("session_id") === "root-slow").length;
  for (let i = 0; i < 6; i++) {
    const slow = { sessionID: "root-slow", system: [] };
    await a.hooks.get("session.context")(slow);
    assert.equal(slow.system.length, 0);
  }
  assert.equal(slowHandoffs(), 3, "a failing handoff fetch must stop retrying after its attempt budget");

  await a.hooks.get("tool.execute.after")({ sessionID: "root-a", tool: "example", id: "t1", input: {},
    status: "completed", result: { content: "content-only tool evidence" } });
  for (const type of ["session.execution.succeeded", "session.execution.failed", "session.execution.interrupted"]) {
    a.emit({ type, data: { sessionID: "root-a" } });
    b.emit({ type, data: { sessionID: "root-a" } });
  }
  a.emit({ type: "session.execution.succeeded", data: { sessionID: "child" } });
  await until(() => requests.filter((r) => r.url.searchParams.get("event") === "stop").length === 4, "completion events delivered");
  const stops = requests.filter((r) => r.url.searchParams.get("event") === "stop");
  assert.equal(stops.filter((r) => r.payload.turn_checkpoint).length, 3);
  assert.equal(stops.find((r) => r.payload.sessionID === "child").payload.agent_id, "root-a");
  assert.ok(requests.some((r) => r.payload?.output === "content-only tool evidence"));
  assert.ok(!requests.some((r) => r.url.searchParams.get("event") === "session-end"));

  // #1983: the compaction request gets one line naming the session, its end
  // arms the anchor for every later model call, and the next compaction's
  // start forgets it. A subagent's compaction gets neither.
  const anchors = () => requests.filter((r) => r.url.pathname === "/recap" && r.url.searchParams.get("mode") === "anchor");
  const stopCount = () => requests.filter((r) => r.url.searchParams.get("event") === "stop").length;
  a.emit({ type: "session.compaction.started", data: { sessionID: "root-a", reason: "auto" } });
  await until(() => requests.some((r) => r.url.searchParams.get("event") === "pre-compact"), "compaction start captured");
  const summaryRequest = { sessionID: "root-a", system: [] };
  await a.hooks.get("session.compaction")(summaryRequest);
  assert.equal(summaryRequest.system.length, 1);
  assert.equal(summaryRequest.system[0].type, "text");
  assert.ok(summaryRequest.system[0].text.includes("(root-a)"), summaryRequest.system[0].text);
  const childSummary = { sessionID: "child", system: [] };
  await a.hooks.get("session.compaction")(childSummary);
  assert.equal(childSummary.system.length, 0, "a subagent compaction gets no note");
  a.emit({ type: "session.compaction.ended", data: { sessionID: "root-a", reason: "auto", text: "summary" } });
  a.emit({ type: "session.compaction.ended", data: { sessionID: "child", reason: "auto", text: "summary" } });
  await until(() => anchors().length === 1, "compaction end arms the anchor");
  assert.equal(anchors()[0].url.searchParams.get("session_id"), "root-a");
  assert.equal(anchors()[0].url.searchParams.get("budget"), "2000");
  for (let i = 0; i < 2; i++) {
    const call = { sessionID: "root-a", system: [] };
    await a.hooks.get("session.context")(call);
    assert.ok(call.system.some((part) => part.text === "ANCHOR root-a"), "anchor rides every model call until the next compaction");
  }
  assert.equal(anchors().length, 1, "the anchor is fetched once per compaction");
  const stopsBefore = stopCount();
  a.emit({ type: "session.compaction.started", data: { sessionID: "root-a", reason: "auto" } });
  a.emit({ type: "session.execution.succeeded", data: { sessionID: "root-a" } });
  await until(() => stopCount() > stopsBefore, "events after the next compaction start processed");
  const afterNext = { sessionID: "root-a", system: [] };
  await a.hooks.get("session.context")(afterNext);
  assert.ok(!afterNext.system.some((part) => part.text === "ANCHOR root-a"), "the next compaction forgets the anchor");
  assert.equal(anchors().length, 1, "a subagent compaction never fetches an anchor");

  await b.hooks.get("session.prompt")({ sessionID: "root-b", messageID: "b1", prompt: { text: "beta" } });
  await b.hooks.get("session.context")({ sessionID: "root-b", system: [] });
  // Deletion, not unload, is the end signal: a deleted child keeps ancestry
  // so its close does not hand the next session a sub-task baton.
  a.emit({ type: "session.deleted", data: { sessionID: "child" } });
  await until(() => requests.some((r) => r.url.searchParams.get("event") === "session-end" && r.payload.sessionID === "child"), "deleted session must end");
  assert.equal(requests.find((r) => r.url.searchParams.get("event") === "session-end" && r.payload.sessionID === "child").payload.agent_id, "root-a", "child close must retain ancestry to suppress automatic handoffs");
  // #1074: OpenCode unloads the plugin when it evicts idle location services;
  // the host and its sessions stay alive, so unload must not end them.
  await disposeA();
  assert.ok(!requests.some((r) => r.url.searchParams.get("event") === "session-end" && r.payload.sessionID === "root-a"), "eviction unload must not end a live session");
  assert.ok(!requests.some((r) => r.url.searchParams.get("event") === "session-end" && r.payload.sessionID === "root-b"), "location cleanup must not close another instance");
  console.log("PASS: resumed sessions, retained handoff, child isolation, bounded handoff retries, terminal events, post-compaction anchor, deletion ends, unload does not");
} finally {
  await disposeB();
}

const spool = join(process.env.AI_MEMORY_DATA_DIR, "hook-spool");
const realNow = Date.now;
globalThis.fetch = async () => { throw new Error("offline fixture"); };
Date.now = () => 1800000000000;
try {
  const offlineA = host(root, records);
  const offlineB = host(root + "/beta", records);
  const closeA = await plugin.setup(offlineA.ctx);
  const closeB = await plugin.setup(offlineB.ctx);
  await Promise.all([
    offlineA.hooks.get("session.prompt")({ sessionID: "root-a", messageID: "offline-a", prompt: { text: "offline alpha" } }),
    offlineB.hooks.get("session.prompt")({ sessionID: "root-b", messageID: "offline-b", prompt: { text: "offline beta" } }),
  ]);
  await Promise.all([closeA(), closeB()]);
  const entries = readdirSync(spool).filter((name) => name.endsWith(".json")).map((name) => JSON.parse(readFileSync(join(spool, name), "utf8")));
  for (const id of ["root-a", "root-b"]) {
    assert.ok(entries.some((entry) => new URL(entry.url).searchParams.get("event") === "session-start" && JSON.parse(entry.body).sessionID === id), `same-millisecond spool must retain ${id}`);
  }
  assert.ok(entries.every((entry) => new URL(entry.url).searchParams.has("ingest_key")), "stable delivery keys survive spooling");
  console.log("PASS: concurrent location spools do not overwrite same-millisecond events");
} finally {
  Date.now = realNow;
}

// A server which accepts requests but never answers must not hold plugin unload
// behind every queued request's individual timeout.
globalThis.fetch = (_url, { signal } = {}) => new Promise((_resolve, reject) => {
  if (signal.aborted) reject(signal.reason);
  else signal.addEventListener("abort", () => reject(signal.reason), { once: true });
});
const stalled = host(root, records);
const closeStalled = await plugin.setup(stalled.ctx);
await stalled.hooks.get("session.prompt")({ sessionID: "root-a", messageID: "stalled", prompt: { text: "bounded shutdown" } });
for (let n = 0; n < 60; n++) {
  await stalled.hooks.get("tool.execute.after")({ sessionID: "root-a", tool: "example", id: `pending-${n}`, input: {}, status: "completed", result: { content: "queued evidence" } });
}
stalled.emit({ type: "session.text.ended", data: { sessionID: "root-a", text: "last response" } });
stalled.emit({ type: "session.execution.succeeded", data: { sessionID: "root-a" } });
await new Promise((resolve) => setImmediate(resolve));
const shutdown = performance.now();
// The plugin's drain timers are unref'd; a real host process stays alive.
const host_alive = setInterval(() => {}, 1000);
await closeStalled();
clearInterval(host_alive);
assert.ok(performance.now() - shutdown < 5000, "unload must not wait for the entire network timeout backlog");
console.log("PASS: unload cancels stalled capture and remains bounded");
