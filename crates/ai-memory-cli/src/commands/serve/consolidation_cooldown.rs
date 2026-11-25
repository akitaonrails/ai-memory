//! Persistent pause for the HTTP server's session-consolidation worker.

use std::path::{Path, PathBuf};
use std::time::Duration;

use ai_memory_llm::{LlmError, LlmResult, ProviderConfig};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::Config;

const STATE_FILE: &str = "session-consolidation-cooldown.json";

#[derive(Deserialize, Serialize)]
struct State {
    route: [u8; 32],
    retry_at: i64,
}

pub(super) struct ConsolidationCooldown {
    path: PathBuf,
    route: [u8; 32],
    retry_at: i64,
    restart_retry_at: i64,
    quota_cooldown: Duration,
}

impl ConsolidationCooldown {
    pub(super) fn open(data_dir: &Path, route: [u8; 32], quota_cooldown: Duration) -> Self {
        let path = data_dir.join(STATE_FILE);
        let state = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<State>(&bytes) {
                Ok(state) => Some(state),
                Err(_) => {
                    tracing::warn!("invalid session consolidation cooldown state");
                    None
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                tracing::warn!(kind = ?error.kind(), "could not read session consolidation cooldown");
                None
            }
        };
        let restart_retry_at = state
            .filter(|state| state.route == route)
            .map_or(0, |state| state.retry_at);
        Self {
            path,
            route,
            retry_at: restart_retry_at,
            restart_retry_at,
            quota_cooldown,
        }
    }

    pub(super) fn billing_delays(&self, error: &LlmError) -> Option<(Duration, Duration)> {
        match error {
            LlmError::FallbackQuotaExhausted {
                retry_after,
                restart_retry_after,
                ..
            } => Some((*retry_after, *restart_retry_after)),
            error if error.is_quota_exhausted() => {
                let delay = error
                    .retry_after()
                    .unwrap_or_default()
                    .max(self.quota_cooldown)
                    .min(Duration::from_secs(3600).max(self.quota_cooldown));
                Some((delay, delay))
            }
            _ => None,
        }
    }

    pub(super) fn remaining(&self, now: i64) -> Option<Duration> {
        let micros = u64::try_from(self.retry_at.checked_sub(now)?).ok()?;
        (micros != 0).then(|| Duration::from_micros(micros))
    }

    pub(super) fn pause(&mut self, now: i64, delay: Duration, restart_delay: Duration) {
        let deadline = |delay: Duration| {
            now.saturating_add(i64::try_from(delay.as_micros()).unwrap_or(i64::MAX))
        };
        // The live chain still knows which candidates can be tried early.
        // After a restart the individual circuits are gone, so keep the
        // latest known deadline until a real queued consolidation succeeds.
        self.retry_at = deadline(delay);
        self.restart_retry_at = self.restart_retry_at.max(deadline(restart_delay));
        self.persist();
    }

    pub(super) fn clear(&mut self, now: i64) {
        if self.restart_retry_at > now {
            self.retry_at = now;
            self.restart_retry_at = now;
            self.persist();
        }
    }

    fn persist(&self) {
        let persisted = serde_json::to_vec(&State {
            route: self.route,
            retry_at: self.restart_retry_at,
        })
        .is_ok_and(|bytes| ai_memory_wiki::write_atomic(&self.path, &bytes).is_ok());
        if !persisted {
            tracing::warn!("could not persist session consolidation cooldown");
        }
    }
}

// Match the effective consolidation chain, including candidate order and the
// names of explicit credential sources. Never read or hash credential values.
pub(super) fn route_fingerprint(config: &Config) -> LlmResult<[u8; 32]> {
    let mut digest = Sha256::new();
    digest.update(b"session-consolidation-route-v1");
    let mut candidate =
        |provider: &ProviderConfig, key_env: Option<&str>, key_file: Option<&Path>| {
            let endpoint = provider
                .base_url
                .as_deref()
                .and_then(|url| reqwest::Url::parse(url).ok());
            let port = endpoint
                .as_ref()
                .and_then(reqwest::Url::port)
                .map_or_else(String::new, |port| port.to_string());
            for part in [
                provider.provider.name(),
                &provider.model,
                endpoint.as_ref().map_or("", |url| url.scheme()),
                endpoint
                    .as_ref()
                    .and_then(|url| url.host_str())
                    .unwrap_or(""),
                &port,
                endpoint.as_ref().map_or("", |url| url.path()),
                key_env.unwrap_or(""),
            ] {
                digest.update((part.len() as u64).to_le_bytes());
                digest.update(part.as_bytes());
            }
            let key_file = key_file.map_or(&[][..], |path| path.as_os_str().as_encoded_bytes());
            digest.update((key_file.len() as u64).to_le_bytes());
            digest.update(key_file);
        };
    if let Some(task) = config.llm_task_configs.get("consolidation") {
        let settings = config.llm.get("consolidation");
        let key_env = settings.and_then(|task| task.api_key_env.as_deref());
        let key_file = if key_env.is_none() {
            config.llm_api_key_file.as_deref()
        } else {
            None
        };
        candidate(&task.primary, key_env, key_file);
        for (index, provider) in task.fallbacks.iter().enumerate() {
            let key_env = settings
                .and_then(|task| task.fallbacks.get(index))
                .and_then(|profile| profile.api_key_env.as_deref());
            candidate(provider, key_env, None);
        }
    } else if let Some(primary) = config.llm_provider_config()? {
        candidate(&primary, None, config.llm_api_key_file.as_deref());
        for (provider, profile) in config
            .llm_fallback_configs
            .iter()
            .zip(&config.llm_fallbacks)
        {
            candidate(provider, profile.api_key_env.as_deref(), None);
        }
    }
    Ok(digest.finalize().into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_honors_the_longest_deadline_until_a_real_success_clears_it() {
        let dir = tempfile::tempdir().unwrap();
        let open = || ConsolidationCooldown::open(dir.path(), [0; 32], Duration::from_secs(15));
        let mut live = open();
        let short = Duration::from_secs(15);
        let long = Duration::from_secs(30);
        let short_deadline = 15_000_000;
        live.pause(0, short, long);
        assert_eq!(live.remaining(short_deadline), None);
        assert_eq!(open().remaining(short_deadline), Some(short));
        assert_eq!(open().remaining(30_000_000), None);

        // A later failed attempt can set a shorter live pause without
        // discarding a previous candidate's still-active longer deadline.
        let shorter_retry = Duration::from_secs(5);
        live.pause(short_deadline, shorter_retry, shorter_retry);
        assert_eq!(live.remaining(20_000_000), None);
        assert_eq!(open().remaining(20_000_000), Some(Duration::from_secs(10)));

        live.clear(20_000_000);
        assert_eq!(open().remaining(20_000_000), None);
    }

    #[test]
    fn route_fingerprint_covers_endpoint_and_ignores_url_credentials() {
        let mut config = Config {
            llm_provider: Some("openai".into()),
            llm_base_url: Some("https://fake-user:fake-secret@example.invalid/v1?key=fake".into()),
            ..Config::default()
        };
        let route = route_fingerprint(&config).unwrap();
        config.llm_base_url =
            Some("https://different-user:different-secret@example.invalid/v1?key=other".into());
        assert_eq!(route_fingerprint(&config).unwrap(), route);
        config.llm_base_url = Some("https://example.invalid/v2".into());
        assert_ne!(route_fingerprint(&config).unwrap(), route);
    }
}
