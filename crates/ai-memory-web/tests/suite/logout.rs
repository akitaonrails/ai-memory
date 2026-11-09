//! The header's "Log out" button, run under Node against a minimal DOM.
//!
//! The button used to navigate to the login page whatever `/auth/logout`
//! answered, and it was shown on a Basic sign-in and on a server without
//! auth, where there is no browser session to end. An operator on a shared
//! browser profile could believe they had signed out while the session, or
//! the cached Basic credential, still worked.

use std::process::Command;

use ai_memory_store::Store;
use ai_memory_web::router;
use ai_memory_wiki::Wiki;
use axum::body::Body;
use axum::http::Request;
use serde_json::Value;
use tempfile::TempDir;
use tower::ServiceExt;

/// A DOM just large enough for the base template's inline script. Reads the
/// script from argv[2] and the scenario from argv[3], clicks the button,
/// and prints what happened.
const HARNESS: &str = r#"
const fs = require("fs");
const script = fs.readFileSync(process.argv[2], "utf8");
const scenario = JSON.parse(process.argv[3]);
const assigned = [], notes = [];
const elements = {};
function element(id) {
  return {
    id, disabled: false, removed: false, dataset: { error: "LOGOUT FAILED" }, listeners: {},
    addEventListener(type, fn) { this.listeners[type] = fn; },
    setAttribute() {}, getAttribute() { return null; }, focus() {},
    querySelector() { return element("child"); }, querySelectorAll() { return []; },
    classList: { toggle() {}, remove() {}, contains() { return false; } },
    remove() { this.removed = true; },
    after(node) { notes.push(node); },
  };
}
global.document = {
  cookie: scenario.cookie,
  body: element("body"),
  addEventListener() {},
  getElementById(id) {
    if (id === "ai-memory-logout-error") return notes.find((n) => !n.removed) || null;
    return (elements[id] = elements[id] || element(id));
  },
  querySelector(selector) {
    if (selector.includes("ai-memory-login-path")) return { content: "/web/login" };
    if (selector.includes("ai-memory-base-path")) return { content: "" };
    return null;
  },
  createElement() {
    return { setAttribute(k, v) { this[k] = v; }, remove() { this.removed = true; } };
  },
};
global.location = {
  href: "http://localhost/web", pathname: "/web", protocol: "http:",
  assign(url) { assigned.push(url); }, reload() {},
};
global.fetch = (url, init) =>
  scenario.status === null
    ? Promise.reject(new Error("network down"))
    : Promise.resolve({ ok: scenario.status >= 200 && scenario.status < 300, status: scenario.status });
new Function(script)();
const button = elements["ai-memory-logout"];
(async () => {
  if (!button.removed && button.listeners.click) {
    button.listeners.click();
    for (let i = 0; i < 5; i++) await new Promise((r) => setTimeout(r, 0));
  }
  process.stdout.write(JSON.stringify({
    removed: button.removed,
    assigned,
    alert: notes.filter((n) => !n.removed).map((n) => n.textContent),
    disabled: button.disabled,
  }));
})();
"#;

fn node_available() -> bool {
    match Command::new("node").arg("--version").output() {
        Ok(output) if output.status.success() => true,
        _ => {
            eprintln!("skipping logout script test: node is not installed");
            false
        }
    }
}

async fn logout_script() -> String {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    let resp = router(store.reader.clone(), wiki)
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();
    html.split("<script>")
        .skip(1)
        .filter_map(|part| part.split("</script>").next())
        .find(|script| script.contains("getElementById(\"ai-memory-logout\")"))
        .expect("the base template carries the logout script")
        .to_owned()
}

fn run(dir: &TempDir, script: &str, cookie: &str, status: Option<u16>) -> Value {
    let script_path = dir.path().join("logout.js");
    let harness_path = dir.path().join("harness.js");
    std::fs::write(&script_path, script).unwrap();
    std::fs::write(&harness_path, HARNESS).unwrap();
    let scenario = serde_json::json!({ "cookie": cookie, "status": status });
    let output = Command::new("node")
        .arg(&harness_path)
        .arg(&script_path)
        .arg(scenario.to_string())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "node failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[tokio::test]
async fn log_out_leaves_the_page_only_when_the_session_is_gone() {
    if !node_available() {
        return;
    }
    let script = logout_script().await;
    let dir = TempDir::new().unwrap();
    let session = "ai_memory_csrf=token123";

    // No browser session (a Basic sign-in, or no auth at all): no button.
    let none = run(&dir, &script, "ai_memory_language=en", Some(204));
    assert_eq!(none["removed"], true, "{none}");
    assert_eq!(none["assigned"], serde_json::json!([]), "{none}");

    // Ended (204), or already gone (401): go to the login page.
    for status in [204, 401] {
        let out = run(&dir, &script, session, Some(status));
        assert_eq!(out["removed"], false, "{status}: {out}");
        assert_eq!(
            out["assigned"],
            serde_json::json!(["/web/login"]),
            "{status}: {out}"
        );
        assert_eq!(out["alert"], serde_json::json!([]), "{status}: {out}");
    }

    // Refused (CSRF 403, a 500) or unreachable: stay, say so, allow a retry.
    for status in [Some(403), Some(500), None] {
        let out = run(&dir, &script, session, status);
        assert_eq!(out["assigned"], serde_json::json!([]), "{status:?}: {out}");
        assert_eq!(
            out["alert"],
            serde_json::json!(["LOGOUT FAILED"]),
            "{status:?}: {out}"
        );
        assert_eq!(out["disabled"], false, "{status:?}: {out}");
    }
}
