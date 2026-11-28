//! `ai-memory healthcheck` — a short, bounded liveness probe for containers.
//!
//! The default target is the host and port from the same `bind` configuration
//! `serve` uses, plus the same normalized base path: a wildcard address maps to
//! loopback, and an IP literal or `localhost` keeps its host. Any other
//! configured hostname is rejected instead of resolved. It never consults the
//! general client `server_url`, resolves credentials, or uses environment
//! proxies. An explicit `--url` selects the authenticated remote-client path.
//! Neither path calls `/admin/status` or opens SQLite. Docker's own
//! `HEALTHCHECK --timeout` remains a second, outer guard.

use std::net::IpAddr;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::cli::HealthcheckArgs;
use crate::config::{Config, server_url_for_bind};
use crate::http_client::{ServerEndpoint, display_url, http_client};

/// Exit successfully only when `/healthz` returns 2xx before the deadline.
pub async fn run(config: &Config, args: HealthcheckArgs) -> Result<()> {
    let timeout = Duration::from_millis(args.timeout_ms);
    let response = tokio::time::timeout(timeout, async {
        if let Some(target) = args.url {
            let endpoint = ServerEndpoint::from_url_resolving_auth(config, target).await;
            let url = endpoint.build_url("/healthz");
            return endpoint
                .authenticate(http_client().get(&url))
                .send()
                .await
                .context(display_url(&url));
        }

        let url = local_url(config)?;
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("building HTTP client")?;
        client.get(&url).send().await.context(display_url(&url))
    })
    .await
    .context("healthcheck timed out")??;
    if !response.status().is_success() {
        bail!("GET /healthz returned {}", response.status());
    }
    Ok(())
}

fn local_url(config: &Config) -> Result<String> {
    let bind = config.bind.trim();
    // Split exactly as `server_url_for_bind` does, so the host checked below is
    // the one that function mapped.
    let (host, _) = bind
        .rsplit_once(':')
        .filter(|(host, _)| !host.is_empty())
        .context("local healthcheck requires a bind address with a non-zero TCP port")?;
    // Reuse the `server_url` derivation so the probe dials the same host the
    // client URL names: a wildcard bind becomes loopback, everything else is
    // kept as given.
    let target = server_url_for_bind(bind)
        .context("local healthcheck requires a bind address with a non-zero TCP port")?;
    // Reject hostnames other than localhost before the HTTP client can resolve
    // them; localhost retains the system resolver's usual behavior.
    let bare_host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    if bare_host.parse::<IpAddr>().is_err() && bare_host != "localhost" {
        bail!(
            "local healthcheck does not derive its target from a hostname in bind; use --url \
             <base-url> to probe explicitly, or set bind to an IP literal or a wildcard address"
        );
    }
    let base_path = ai_memory_web::normalize_prefix(&config.base_path);
    Ok(format!("{target}{base_path}/healthz"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_target_maps_each_bind_host_and_keeps_the_normalized_base_path() {
        let mut config = Config {
            base_path: "/wiki/".to_owned(),
            ..Config::default()
        };
        for (bind, expected) in [
            ("127.0.0.1:1234", "http://127.0.0.1:1234/wiki/healthz"),
            ("0.0.0.0:1234", "http://127.0.0.1:1234/wiki/healthz"),
            ("[::]:1234", "http://[::1]:1234/wiki/healthz"),
            ("[::1]:1234", "http://[::1]:1234/wiki/healthz"),
            ("localhost:1234", "http://localhost:1234/wiki/healthz"),
            ("192.0.2.10:1234", "http://192.0.2.10:1234/wiki/healthz"),
        ] {
            config.bind = bind.to_owned();
            config.server_url = "http://192.0.2.10:9999/other".to_owned();
            assert_eq!(local_url(&config).unwrap(), expected, "{bind}");
        }
    }

    #[test]
    fn local_target_rejects_a_hostname_instead_of_resolving_it() {
        let config = Config {
            bind: "memory.lan:1234".to_owned(),
            ..Config::default()
        };
        let error = local_url(&config).unwrap_err().to_string();
        assert!(
            error.contains("does not derive its target from a hostname"),
            "{error}"
        );
    }

    #[test]
    fn local_target_rejects_a_missing_or_dynamic_port() {
        for bind in [
            "",
            "localhost",
            ":1234",
            "localhost:0",
            "localhost:not-a-port",
            // A bare IPv6 literal has no host a URL can carry, so it must not
            // fall back to loopback either.
            "::1:1234",
        ] {
            let config = Config {
                bind: bind.to_owned(),
                ..Config::default()
            };
            assert!(local_url(&config).is_err(), "{bind}");
        }
    }
}
