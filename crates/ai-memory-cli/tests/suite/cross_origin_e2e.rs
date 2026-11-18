//! Adversarial end-to-end test: a web page the operator visits must not be
//! able to write to an unauthenticated loopback `ai-memory serve`.
//!
//! The documented default is `127.0.0.1:49374` with no bearer token. Any page
//! in the operator's browser can address that origin. Browsers send "simple"
//! requests cross-origin without a CORS preflight: `GET`, and `POST` whose
//! `Content-Type` is `text/plain`, `application/x-www-form-urlencoded` or
//! `multipart/form-data`, carrying no custom headers. The page cannot read the
//! response, but the server still acts on the request. DNS rebinding goes
//! further: the attacker's own hostname resolves to `127.0.0.1`, so the
//! request is same-origin for the browser and only the `Host` header gives it
//! away.
//!
//! Three server guards stand in the way (`docs/security-boundaries.md` row
//! 12c):
//!
//! 1. The browser-origin guard on the machine and admin routes
//!    (`serve::refuse_cross_origin_browser_requests`) refuses a request a
//!    browser marks as cross-site (`Sec-Fetch-Site`) or, from a browser too old
//!    to send fetch metadata, one whose `Origin` names another host. It also
//!    refuses any request whose `Sec-Fetch-Dest` is not `empty` (a link,
//!    image or navigation rather than script `fetch()`). Hooks, the CLI and
//!    MCP clients send none of these headers.
//! 2. JSON-only extractors (axum `Json`, rmcp's `Content-Type` check) refuse
//!    the three simple content types with 415, so a preflight-free `POST`
//!    cannot deliver a body even past guard 1.
//! 3. The `Host` allowlist refuses a rebinding hostname with 403.
//! 4. On a host a browser sends no fetch metadata to (a LAN address or name in
//!    the allowlist, over plain HTTP), an `<img>` request carries neither
//!    fetch metadata nor `Origin`. There the handoff fetch must also carry the
//!    `X-AI-Memory-Client` marker every first-party client sends and a page
//!    cannot add; loopback hosts keep accepting legacy hooks without it.
//!
//! Every attack carries a canary string. After the attacks the test scans the
//! whole data directory, SQLite and wiki alike, for it, and checks that the
//! destructive targets (a page, a project, a single-use handoff) survived. The
//! legitimate controls send the same requests the way hooks, the CLI and MCP
//! clients do (`application/json`, no `Origin`) and must succeed.

/// Spawns a server: seconds, not milliseconds, so the slow tier.
mod slow {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use reqwest::{Method, RequestBuilder, StatusCode};
    use serde_json::{Value, json};

    use crate::e2e_support::{hermetic, session_count, start_serve};

    const BIN: &str = env!("CARGO_BIN_EXE_ai-memory");
    const WORKSPACE: &str = "a0-ws";
    /// The operator's real project: the target of the destructive attacks.
    const PROJECT: &str = "a0-project";
    /// Where the anonymous active-project pointer sits before the attacks.
    const BASELINE_PROJECT: &str = "a0-baseline";
    /// A project only an attack would create.
    const EVIL_PROJECT: &str = "a0-evil-project";
    const PAGE: &str = "notes/keep-me.md";
    const CANARY: &str = "a0-cross-origin-canary-7f3c";
    const CONTROL: &str = "a0-legit-control-marker-91be";
    const HANDOFF_SUMMARY: &str = "a0 handoff baton that must survive";
    const EVIL_ORIGIN: &str = "https://evil.example";

    /// What a browser attaches to a request a foreign page issues. Real
    /// browsers send both headers; the single-header shapes cover a browser
    /// without fetch metadata (Origin only) and a no-cors `GET` such as an
    /// `<img>` tag, which carries fetch metadata but no `Origin`.
    #[derive(Clone, Copy, Debug)]
    enum Browser {
        Modern,
        LegacyOriginOnly,
        FetchMetadataOnly,
    }

    const BROWSERS: [Browser; 3] = [
        Browser::Modern,
        Browser::LegacyOriginOnly,
        Browser::FetchMetadataOnly,
    ];

    fn from_evil_page(req: RequestBuilder, browser: Browser) -> RequestBuilder {
        match browser {
            Browser::Modern => req
                .header("origin", EVIL_ORIGIN)
                .header("sec-fetch-site", "cross-site")
                .header("sec-fetch-mode", "no-cors"),
            Browser::LegacyOriginOnly => req.header("origin", EVIL_ORIGIN),
            Browser::FetchMetadataOnly => req
                .header("sec-fetch-site", "cross-site")
                .header("sec-fetch-mode", "no-cors"),
        }
    }

    /// The three content types a browser sends cross-origin without a preflight.
    const SIMPLE_CONTENT_TYPES: [&str; 3] = [
        "text/plain;charset=UTF-8",
        "application/x-www-form-urlencoded",
        "multipart/form-data; boundary=----a0boundary",
    ];

    fn tree_contains(root: &Path, needle: &str) -> Option<PathBuf> {
        let mut pending = vec![root.to_path_buf()];
        while let Some(dir) = pending.pop() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    pending.push(path);
                } else if let Ok(bytes) = fs::read(&path)
                    && bytes
                        .windows(needle.len())
                        .any(|window| window == needle.as_bytes())
                {
                    return Some(path);
                }
            }
        }
        None
    }

    fn mcp_call(name: &str, arguments: Value) -> String {
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": name, "arguments": arguments },
        })
        .to_string()
    }

    /// A legitimate MCP client call: JSON in, both accept types, no Origin.
    async fn legit_mcp(client: &reqwest::Client, base: &str, name: &str, args: Value) -> Value {
        let text = client
            .post(format!("{base}/mcp"))
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(mcp_call(name, args))
            .send()
            .await
            .expect("mcp request")
            .text()
            .await
            .expect("mcp body");
        let reply: Value = serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("non-JSON mcp reply {text}: {e}"));
        assert!(
            reply.get("error").is_none() && reply.pointer("/result/isError") != Some(&json!(true)),
            "legitimate MCP call {name} must succeed: {text}"
        );
        reply
    }

    /// One write a foreign page would attempt, with a JSON body that would do
    /// damage if the server parsed it.
    struct Attack {
        label: &'static str,
        method: Method,
        path: String,
        body: String,
        /// rmcp answers 406 before 415 without this; a page can send it, since
        /// `Accept` is a CORS-safelisted header.
        mcp_accept: bool,
    }

    fn write_attacks() -> Vec<Attack> {
        let post = |label, path: &str, body: Value| Attack {
            label,
            method: Method::POST,
            path: path.to_string(),
            body: body.to_string(),
            mcp_accept: false,
        };
        let hook_query = format!(
            "event=user-prompt-submit&agent=claude-code&workspace={WORKSPACE}&project={EVIL_PROJECT}"
        );
        let mut attacks = vec![
            post(
                "hook",
                &format!("/hook?{hook_query}"),
                json!({
                    "session_id": "00000000-0000-4000-8000-00000000a0a0",
                    "cwd": "/tmp/a0-evil",
                    "prompt": CANARY,
                }),
            ),
            post(
                "hook batch",
                "/hook/batch",
                json!([{
                    "url": format!("http://h/hook?{hook_query}"),
                    "body": {
                        "session_id": "00000000-0000-4000-8000-00000000a0a1",
                        "cwd": "/tmp/a0-evil",
                        "prompt": CANARY,
                    },
                }]),
            ),
            post(
                "admin write-page",
                "/admin/write-page",
                json!({
                    "workspace": WORKSPACE,
                    "project": EVIL_PROJECT,
                    "path": "notes/evil.md",
                    "body": CANARY,
                }),
            ),
            post(
                "admin delete-page",
                "/admin/delete-page",
                json!({ "workspace": WORKSPACE, "project": PROJECT, "path": PAGE }),
            ),
            post(
                "admin purge-project",
                "/admin/purge-project",
                json!({ "workspace": WORKSPACE, "project": PROJECT, "confirm": true, "force": true }),
            ),
            post(
                "admin messages send",
                "/admin/messages/send",
                json!({
                    "from_workspace": WORKSPACE,
                    "from_project": PROJECT,
                    "to_workspace": WORKSPACE,
                    "to_project": PROJECT,
                    "body": CANARY,
                }),
            ),
            post(
                "messages send",
                "/messages/send",
                json!({
                    "from_workspace": WORKSPACE,
                    "from_project": PROJECT,
                    "to_workspace": WORKSPACE,
                    "to_project": PROJECT,
                    "body": CANARY,
                }),
            ),
            post(
                "workstream prepare run",
                "/workstream/runs",
                json!({ "workspace": WORKSPACE, "project": EVIL_PROJECT, "note": CANARY }),
            ),
        ];
        for (label, name, args) in [
            (
                "mcp write_page",
                "memory_write_page",
                json!({
                    "workspace": WORKSPACE,
                    "project": EVIL_PROJECT,
                    "path": "notes/evil-mcp.md",
                    "body": CANARY,
                }),
            ),
            (
                "mcp delete_page",
                "memory_delete_page",
                json!({ "workspace": WORKSPACE, "project": PROJECT, "path": PAGE }),
            ),
            (
                "mcp message_send",
                "memory_message_send",
                json!({
                    "workspace": WORKSPACE,
                    "project": PROJECT,
                    "to_workspace": WORKSPACE,
                    "to_project": PROJECT,
                    "body": CANARY,
                }),
            ),
            (
                "mcp handoff_accept",
                "memory_handoff_accept",
                json!({ "workspace": WORKSPACE, "project": PROJECT }),
            ),
        ] {
            attacks.push(Attack {
                label,
                method: Method::POST,
                path: "/mcp".to_string(),
                body: mcp_call(name, args),
                mcp_accept: true,
            });
        }
        attacks
    }

    /// Requests a page can issue with no body at all, so the JSON extractors
    /// never see them: only the browser-origin guard stops these.
    fn bodyless_attacks() -> Vec<Attack> {
        let bare = |label, method, path: String| Attack {
            label,
            method,
            path,
            body: String::new(),
            mcp_accept: false,
        };
        vec![
            // The session-start handoff fetch CLAIMS the single-use baton and
            // publishes the caller's active project: a state change on GET,
            // reachable from an `<img src>`.
            bare(
                "handoff claim",
                Method::GET,
                format!("/handoff?agent=claude-code&workspace={WORKSPACE}&project={PROJECT}"),
            ),
            bare(
                "admin profile rebuild",
                Method::POST,
                "/admin/profile/rebuild".to_string(),
            ),
            bare("admin backup", Method::POST, "/admin/backup".to_string()),
        ]
    }

    fn serve(data_dir: &Path, home: &Path, port: u16) -> std::process::Command {
        let mut cmd = hermetic(BIN);
        cmd.env("HOME", home)
            .env("USERPROFILE", home)
            .env("AI_MEMORY_HOME", home)
            .env("AI_MEMORY_DATA_DIR", data_dir)
            .env("AI_MEMORY_EMBEDDING_PROVIDER", "none")
            .env("AI_MEMORY_BACKFILL_ON_START", "false")
            .env("AI_MEMORY_AUTO_IMPROVE__SCHEDULER__ENABLED", "false")
            .current_dir(home)
            .args(["serve", "--transport", "http", "--bind"])
            .arg(format!("127.0.0.1:{port}"))
            .arg("--no-watcher");
        cmd
    }

    async fn open_handoffs(client: &reqwest::Client, base: &str) -> usize {
        let body: Value = client
            .get(format!("{base}/admin/handoffs"))
            .query(&[("workspace", WORKSPACE), ("project", PROJECT)])
            .send()
            .await
            .expect("handoffs request")
            .json()
            .await
            .expect("handoffs json");
        body["handoffs"].as_array().map_or(0, Vec::len)
    }

    /// The scope an unscoped MCP call resolves to, which for this anonymous
    /// caller is the active-project pointer a `/handoff` fetch publishes.
    async fn unscoped_scope(client: &reqwest::Client, base: &str) -> (String, String) {
        let reply = legit_mcp(client, base, "memory_status", json!({})).await;
        let text = reply
            .pointer("/result/content/0/text")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("memory_status reply without text: {reply}"));
        let status: Value = serde_json::from_str(text)
            .unwrap_or_else(|e| panic!("non-JSON memory_status {text}: {e}"));
        let field = |name: &str| {
            status
                .pointer(&format!("/scope/{name}"))
                .and_then(Value::as_str)
                .unwrap_or_else(|| panic!("memory_status without scope.{name}: {text}"))
                .to_string()
        };
        (field("workspace"), field("project"))
    }

    /// The anonymous hook-style `/handoff` fetch: no Origin, no fetch metadata.
    async fn legit_handoff_fetch(client: &reqwest::Client, base: &str, project: &str) -> String {
        let resp = client
            .get(format!("{base}/handoff"))
            .query(&[
                ("agent", "claude-code"),
                ("workspace", WORKSPACE),
                ("project", project),
            ])
            .send()
            .await
            .expect("legit handoff fetch");
        assert_eq!(resp.status(), StatusCode::OK);
        resp.text().await.expect("handoff body")
    }

    async fn page_exists(client: &reqwest::Client, base: &str) -> bool {
        client
            .get(format!("{base}/admin/read-page"))
            .query(&[
                ("workspace", WORKSPACE),
                ("project", PROJECT),
                ("path", PAGE),
            ])
            .send()
            .await
            .expect("read-page request")
            .status()
            .is_success()
    }

    /// A LAN name and a LAN address the operator put in the allowlist.
    const LAN_NAME: &str = "a0-memory.lan";
    const LAN_ADDR: &str = "192.168.77.5";

    async fn handoff_status(
        client: &reqwest::Client,
        base: &str,
        method: Method,
        host: &str,
        marker: Option<&str>,
    ) -> (StatusCode, String) {
        let mut req = client
            .request(
                method,
                format!("{base}/handoff?agent=claude-code&workspace={WORKSPACE}&project={PROJECT}"),
            )
            .header("host", host);
        if let Some(marker) = marker {
            req = req.header("x-ai-memory-client", marker);
        }
        let resp = req.send().await.expect("handoff request");
        let status = resp.status();
        (status, resp.text().await.expect("handoff body"))
    }

    async fn begin_handoff(client: &reqwest::Client, base: &str) {
        legit_mcp(
            client,
            base,
            "memory_handoff_begin",
            json!({
                "workspace": WORKSPACE,
                "project": PROJECT,
                "summary": HANDOFF_SUMMARY,
                "shared": true,
            }),
        )
        .await;
        assert_eq!(open_handoffs(client, base).await, 1, "baton must be open");
    }

    #[tokio::test]
    async fn handoff_fetch_on_a_lan_host_requires_the_client_marker() {
        let root = tempfile::tempdir().expect("tempdir");
        let data_dir = root.path().join("data");
        let home = root.path().join("home");
        fs::create_dir_all(&home).expect("home");
        let client = reqwest::Client::new();
        let (_server, base) = start_serve(&client, &root.path().join("serve.log"), |port| {
            let mut cmd = serve(&data_dir, &home, port);
            cmd.env(
                "AI_MEMORY_ALLOWED_HOSTS",
                format!("localhost,127.0.0.1,::1,{LAN_NAME},{LAN_ADDR}"),
            );
            cmd
        })
        .await;
        let port = base.rsplit(':').next().expect("port").to_string();
        let lan_hosts = [format!("{LAN_NAME}:{port}"), format!("{LAN_ADDR}:{port}")];

        legit_mcp(
            &client,
            &base,
            "memory_write_page",
            json!({
                "workspace": WORKSPACE,
                "project": BASELINE_PROJECT,
                "path": "notes/baseline.md",
                "body": "Baseline project.",
            }),
        )
        .await;
        legit_handoff_fetch(&client, &base, BASELINE_PROJECT).await;
        let baseline = (WORKSPACE.to_string(), BASELINE_PROJECT.to_string());
        begin_handoff(&client, &base).await;

        // Attack: what an `<img>` on a foreign page sends to a LAN host over
        // plain HTTP: no fetch metadata, no Origin, no marker.
        for host in &lan_hosts {
            for method in [Method::GET, Method::HEAD] {
                let (status, body) =
                    handoff_status(&client, &base, method.clone(), host, None).await;
                assert_eq!(
                    status,
                    StatusCode::FORBIDDEN,
                    "{method} /handoff via {host} without the marker must be refused"
                );
                if method == Method::GET {
                    assert!(
                        body.contains("install-hooks") && body.contains("AI_MEMORY_AUTH_TOKEN"),
                        "the refusal must tell the operator what to do: {body}"
                    );
                }
            }
            // An empty marker is no marker.
            let (status, _) = handoff_status(&client, &base, Method::GET, host, Some("")).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "empty marker via {host}");
        }
        assert_eq!(
            open_handoffs(&client, &base).await,
            1,
            "a refused LAN fetch must not claim the baton"
        );
        assert_eq!(
            unscoped_scope(&client, &base).await,
            baseline,
            "a refused LAN fetch must not move the active-project pointer"
        );

        // Control: a current hook names itself and claims through the LAN name.
        let (status, body) =
            handoff_status(&client, &base, Method::GET, &lan_hosts[0], Some("hook")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            body.contains(HANDOFF_SUMMARY),
            "marked fetch claims: {body}"
        );
        assert_eq!(open_handoffs(&client, &base).await, 0);

        // Legacy hooks on a loopback install send no marker and must keep
        // working: browsers send fetch metadata to loopback, which guard 1
        // already checks.
        begin_handoff(&client, &base).await;
        let (status, body) = handoff_status(
            &client,
            &base,
            Method::GET,
            &format!("127.0.0.1:{port}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            body.contains(HANDOFF_SUMMARY),
            "legacy loopback fetch: {body}"
        );
        assert_eq!(open_handoffs(&client, &base).await, 0);

        // Attack: a plain-HTTP proxy that rewrites `Host` to the loopback
        // address. The browser behind it addressed a non-loopback origin, so
        // it sent no fetch metadata; the proxy's forwarding header gives the
        // hop away.
        begin_handoff(&client, &base).await;
        let loopback = format!("127.0.0.1:{port}");
        let proxied = |proxy_header: &'static str, marker: Option<&'static str>| {
            let mut req = client
                .get(format!(
                    "{base}/handoff?agent=claude-code&workspace={WORKSPACE}&project={PROJECT}"
                ))
                .header("host", &loopback)
                .header(proxy_header, "203.0.113.7");
            if let Some(marker) = marker {
                req = req.header("x-ai-memory-client", marker);
            }
            req.send()
        };
        for proxy_header in [
            "forwarded",
            "x-forwarded-for",
            "x-forwarded-host",
            "x-real-ip",
            "via",
        ] {
            let resp = proxied(proxy_header, None).await.expect("proxied fetch");
            assert_eq!(
                resp.status(),
                StatusCode::FORBIDDEN,
                "loopback Host behind a proxy ({proxy_header}) without the marker"
            );
        }
        assert_eq!(
            open_handoffs(&client, &base).await,
            1,
            "a refused proxied fetch must not claim the baton"
        );
        // Control: a current hook behind the same proxy names itself.
        let resp = proxied("x-forwarded-for", Some("hook"))
            .await
            .expect("marked proxied fetch");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.text().await.expect("handoff body");
        assert!(
            body.contains(HANDOFF_SUMMARY),
            "marked proxied fetch: {body}"
        );
        assert_eq!(open_handoffs(&client, &base).await, 0);
    }

    #[tokio::test]
    async fn browser_cross_origin_requests_cannot_write_to_an_unauthenticated_loopback_server() {
        let root = tempfile::tempdir().expect("tempdir");
        let data_dir = root.path().join("data");
        let home = root.path().join("home");
        fs::create_dir_all(&home).expect("home");
        let client = reqwest::Client::new();
        // The log lives outside the data dir so the canary scan sees only
        // what the server persisted.
        let (_server, base) = start_serve(&client, &root.path().join("serve.log"), |port| {
            serve(&data_dir, &home, port)
        })
        .await;
        let port = base.rsplit(':').next().expect("port").to_string();

        // Legitimate controls first: an MCP client (JSON, no Origin) writes the
        // page and the handoff the attacks will go after.
        legit_mcp(
            &client,
            &base,
            "memory_write_page",
            json!({
                "workspace": WORKSPACE,
                "project": PROJECT,
                "path": PAGE,
                "body": format!("Keep this page. {CONTROL}"),
            }),
        )
        .await;
        legit_mcp(
            &client,
            &base,
            "memory_handoff_begin",
            json!({
                "workspace": WORKSPACE,
                "project": PROJECT,
                "summary": HANDOFF_SUMMARY,
                "shared": true,
            }),
        )
        .await;
        // Park the anonymous active-project pointer on another project, the way
        // a hook's session-start fetch does, so a claim that moved it shows.
        legit_mcp(
            &client,
            &base,
            "memory_write_page",
            json!({
                "workspace": WORKSPACE,
                "project": BASELINE_PROJECT,
                "path": "notes/baseline.md",
                "body": "Baseline project.",
            }),
        )
        .await;
        legit_handoff_fetch(&client, &base, BASELINE_PROJECT).await;
        let baseline = (WORKSPACE.to_string(), BASELINE_PROJECT.to_string());
        assert_eq!(
            unscoped_scope(&client, &base).await,
            baseline,
            "the hook-style fetch must publish the active project"
        );
        assert!(page_exists(&client, &base).await, "control page must exist");
        assert_eq!(
            open_handoffs(&client, &base).await,
            1,
            "control handoff must be open"
        );

        // Preflight-free simple requests carrying a damaging JSON body.
        for attack in write_attacks() {
            for content_type in SIMPLE_CONTENT_TYPES {
                for browser in BROWSERS {
                    let mut req = client
                        .request(attack.method.clone(), format!("{base}{}", attack.path))
                        .header("content-type", content_type)
                        .body(attack.body.clone());
                    if attack.mcp_accept {
                        req = req.header("accept", "application/json, text/event-stream");
                    }
                    let status = from_evil_page(req, browser)
                        .send()
                        .await
                        .expect("attack request")
                        .status();
                    assert!(
                        status.is_client_error(),
                        "{} as {content_type} from {browser:?} must be refused, got {status}",
                        attack.label
                    );
                }
            }
        }

        // Bodyless requests: no content type for a JSON extractor to refuse.
        for attack in bodyless_attacks() {
            for browser in BROWSERS {
                let req = client.request(attack.method.clone(), format!("{base}{}", attack.path));
                let resp = from_evil_page(req, browser)
                    .send()
                    .await
                    .expect("attack request");
                assert_eq!(
                    resp.status(),
                    StatusCode::FORBIDDEN,
                    "{} from {browser:?} must be refused by the browser-origin guard",
                    attack.label
                );
            }
        }

        // DNS rebinding: same-origin for the browser, foreign Host for us. The
        // request is a plain JSON one, so only the Host allowlist stops it.
        let rebound_host = format!("rebind.evil.example:{port}");
        for (path, body) in [
            (
                format!("/hook?event=session-start&agent=claude-code&workspace={WORKSPACE}&project={EVIL_PROJECT}"),
                json!({ "session_id": "00000000-0000-4000-8000-00000000a0a2", "cwd": "/tmp/a0", "prompt": CANARY }).to_string(),
            ),
            (
                "/admin/write-page".to_string(),
                json!({ "workspace": WORKSPACE, "project": EVIL_PROJECT, "path": "notes/rebind.md", "body": CANARY }).to_string(),
            ),
            (
                "/mcp".to_string(),
                mcp_call("memory_write_page", json!({ "workspace": WORKSPACE, "project": EVIL_PROJECT, "path": "notes/rebind-mcp.md", "body": CANARY })),
            ),
        ] {
            let status = client
                .post(format!("{base}{path}"))
                .header("host", &rebound_host)
                .header("content-type", "application/json")
                .header("accept", "application/json, text/event-stream")
                .body(body)
                .send()
                .await
                .expect("rebinding request")
                .status();
            assert_eq!(status, StatusCode::FORBIDDEN, "rebound Host on {path} must be refused");
        }
        let status = client
            .get(format!(
                "{base}/handoff?agent=claude-code&workspace={WORKSPACE}&project={PROJECT}"
            ))
            .header("host", &rebound_host)
            .send()
            .await
            .expect("rebinding handoff request")
            .status();
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "rebound Host on /handoff must be refused"
        );

        // A preflight must never come back permissive, so a non-simple request
        // (application/json, custom headers, DELETE) stays blocked in the browser.
        for path in [
            "/hook",
            "/mcp",
            "/admin/write-page",
            "/handoff",
            "/workstream/runs",
        ] {
            let resp = client
                .request(Method::OPTIONS, format!("{base}{path}"))
                .header("origin", EVIL_ORIGIN)
                .header("sec-fetch-site", "cross-site")
                .header("sec-fetch-mode", "cors")
                .header("access-control-request-method", "POST")
                .header("access-control-request-headers", "content-type")
                .send()
                .await
                .expect("preflight request");
            for name in [
                "access-control-allow-origin",
                "access-control-allow-credentials",
                "access-control-allow-headers",
                "access-control-allow-methods",
            ] {
                assert!(
                    resp.headers().get(name).is_none(),
                    "preflight to {path} must not answer {name}: {:?}",
                    resp.headers().get(name)
                );
            }
        }

        // A browser request that is not script fetch(): a link stored in a
        // page (same-origin navigation or image) or a link opened from another
        // application (`Sec-Fetch-Site: none`). The origin check alone lets
        // these through; the destination check refuses them.
        let handoff_url =
            format!("{base}/handoff?agent=claude-code&workspace={WORKSPACE}&project={PROJECT}");
        for (site, dest) in [
            ("same-origin", "document"),
            ("same-origin", "image"),
            ("none", "document"),
        ] {
            let status = client
                .get(&handoff_url)
                .header("sec-fetch-site", site)
                .header("sec-fetch-mode", "navigate")
                .header("sec-fetch-dest", dest)
                .send()
                .await
                .expect("non-fetch browser request")
                .status();
            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "/handoff with Sec-Fetch-Site {site} and Sec-Fetch-Dest {dest} must be refused"
            );
        }
        assert_eq!(
            open_handoffs(&client, &base).await,
            1,
            "a non-fetch browser request must not claim the baton"
        );

        // Checked before the legitimate session-start hook below, which
        // publishes the project itself.
        assert_eq!(
            unscoped_scope(&client, &base).await,
            baseline,
            "the refused /handoff fetches must not move the active-project pointer"
        );

        // Legitimate hook: JSON, no Origin, accepted. Waiting for it to commit
        // also gives any wrongly accepted attack hook time to land.
        let status = client
            .post(format!("{base}/hook"))
            .query(&[
                ("event", "session-start"),
                ("agent", "claude-code"),
                ("workspace", WORKSPACE),
                ("project", PROJECT),
            ])
            .header("content-type", "application/json")
            .body(
                json!({
                    "session_id": "00000000-0000-4000-8000-00000000a0ff",
                    "cwd": "/tmp/a0-project",
                })
                .to_string(),
            )
            .send()
            .await
            .expect("legit hook")
            .status();
        assert_eq!(
            status,
            StatusCode::ACCEPTED,
            "legitimate hook must be accepted"
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while session_count(&client, &base, WORKSPACE, PROJECT).await == 0 {
            assert!(Instant::now() < deadline, "legitimate hook never committed");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // Nothing an attack carried was persisted, and every target survived.
        assert_eq!(
            session_count(&client, &base, WORKSPACE, EVIL_PROJECT).await,
            0,
            "no attack hook may create a session"
        );
        assert!(
            page_exists(&client, &base).await,
            "the page must survive the delete/purge attacks"
        );
        assert_eq!(
            open_handoffs(&client, &base).await,
            1,
            "the cross-origin handoff fetch must not claim the baton"
        );
        let projects = client
            .get(format!("{base}/admin/projects"))
            .send()
            .await
            .expect("projects request")
            .text()
            .await
            .expect("projects body");
        assert!(
            !projects.contains(EVIL_PROJECT),
            "no attack may create a project: {projects}"
        );
        assert!(
            tree_contains(&data_dir, CONTROL).is_some(),
            "the scan must see legitimately written content"
        );
        assert_eq!(
            tree_contains(&data_dir, CANARY),
            None,
            "an attack payload reached the store"
        );

        // The web UI's own calls into /admin are same-origin and must pass.
        let own_origin = base.trim_end_matches('/').to_string();
        let same_origin = |req: RequestBuilder| {
            req.header("origin", &own_origin)
                .header("sec-fetch-site", "same-origin")
                .header("sec-fetch-mode", "cors")
                .header("sec-fetch-dest", "empty")
        };
        let scope = [("workspace", WORKSPACE), ("project", PROJECT)];
        let list = same_origin(client.get(format!("{base}/admin/pending-writes")))
            .query(&scope)
            .send()
            .await
            .expect("same-origin pending-writes list");
        assert_eq!(
            list.status(),
            StatusCode::OK,
            "the web UI's same-origin list must pass"
        );
        let approve = same_origin(client.post(format!(
            "{base}/admin/pending-writes/00000000-0000-4000-8000-00000000a0a9/approve"
        )))
        .query(&scope)
        .header("content-type", "application/json")
        .header("accept", "application/json")
        .body("{}")
        .send()
        .await
        .expect("same-origin pending-write approve");
        // The handler, not the guard's 403, answers the unknown proposal.
        let status = approve.status();
        let body = approve.text().await.expect("approve body");
        assert!(
            status == StatusCode::NOT_FOUND && serde_json::from_str::<Value>(&body).is_ok(),
            "the web UI's same-origin approve must reach the handler: {status} {body}"
        );

        // The hook-style handoff fetch (no Origin) still claims the baton and
        // publishes the project it claimed from.
        let text = legit_handoff_fetch(&client, &base, PROJECT).await;
        assert!(
            text.contains(HANDOFF_SUMMARY),
            "legitimate fetch must receive the baton: {text}"
        );
        assert_eq!(
            open_handoffs(&client, &base).await,
            0,
            "legitimate fetch claims it"
        );
        assert_eq!(
            unscoped_scope(&client, &base).await,
            (WORKSPACE.to_string(), PROJECT.to_string()),
            "the legitimate fetch moves the pointer, so the check above can see a move"
        );
    }
}
