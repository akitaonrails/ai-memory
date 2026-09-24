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
  return new Response(url.pathname === "/handoff" ? "Remember the verified handoff." : "{}");
};
const { default: plugin } = await import(pathToFileURL(process.argv[2]).href);

// Plugin-scoped durable storage outlives any one loaded instance.
const storage = new Map();

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
      storage: {
        async get(key) { return storage.get(key); },
        async set(key, value) { storage.set(key, structuredClone(value)); },
        async remove(key) { storage.delete(key); },
      },
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
  ["child", info("child", "root-a")]]);
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

  await b.hooks.get("session.prompt")({ sessionID: "root-b", messageID: "b1", prompt: { text: "beta" } });
  await b.hooks.get("session.context")({ sessionID: "root-b", system: [] });
  const ends = () => requests.filter((r) => r.url.searchParams.get("event") === "session-end");
  a.emit({ type: "session.deleted", data: { sessionID: "child" } });
  await until(() => ends().length === 1, "deletion ends the session");
  assert.equal(ends()[0].payload.sessionID, "child");
  assert.equal(ends()[0].payload.agent_id, "root-a", "child close must retain ancestry to suppress automatic handoffs");
  await disposeA();
  assert.equal(ends().length, 1, "unload leaves sessions resumable: only deletion ends one");

  // After a reload, deleting a session the new instance never touched still
  // ends it, once, from the location that captured it.
  const reloaded = host(root, records);
  const disposeReloaded = await plugin.setup(reloaded.ctx);
  try {
    reloaded.emit({ type: "session.deleted", data: { sessionID: "root-a" } });
    b.emit({ type: "session.deleted", data: { sessionID: "root-a" } });
    await until(() => ends().length === 2, "deletion after reload ends the session");
    const end = ends()[1];
    assert.equal(end.payload.sessionID, "root-a");
    assert.equal(end.payload.cwd, root);
    assert.equal(end.payload.agent_id, undefined, "a root session carries no ancestry");
    await new Promise((resolve) => setTimeout(resolve, 50));
    assert.equal(ends().length, 2, "another location must not end it too");
    assert.equal(storage.has("ai-memory/session/root-a"), false, "the ended session's record is dropped");
  } finally {
    await disposeReloaded();
  }
  console.log("PASS: resumed sessions, retained handoff, child isolation, terminal events, content-only output, unload keeps sessions open, deletion after reload");
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
