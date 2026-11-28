//! Redirect policy for provider and embedder clients.
//!
//! reqwest's default policy follows up to ten redirects and strips only
//! `Authorization`, cookies and the proxy headers when the host changes.
//! Provider credentials that travel in headers of their own (`x-api-key`,
//! `x-goog-api-key`, operator headers from `llm_headers`) survive that hop,
//! so a configured endpoint that redirects elsewhere would hand them to a
//! host the operator never configured. Following redirects only inside the
//! origin the request was sent to keeps path rewrites working and refuses
//! the rest, like the relay's `Policy::none()` does for its batches.

/// Same hop limit as reqwest's default policy.
const MAX_REDIRECTS: usize = 10;

/// Follow a redirect only when it stays on HTTPS, and cap the hop count.
///
/// Cross-origin hops are allowed here on purpose: a model host such as
/// `huggingface.co` redirects its files to a CDN on another host, and the
/// caller checks every byte against a pinned SHA-256 before the atomic rename.
/// What this refuses is an `https` to `http` downgrade, which would put the
/// request on the wire in the clear.
pub(crate) fn https_only_redirects() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() > MAX_REDIRECTS {
            attempt.error("too many redirects")
        } else if attempt.url().scheme() != "https" {
            let target = attempt.url().origin().ascii_serialization();
            attempt.error(format!(
                "refused a redirect to a non-https origin ({target}); the model host must redirect over https"
            ))
        } else {
            attempt.follow()
        }
    })
}

/// Follow a redirect only when it stays on the scheme, host and port of the
/// previous request.
pub(crate) fn same_origin_redirects() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        let same_origin = attempt
            .previous()
            .last()
            .is_some_and(|previous| previous.origin() == attempt.url().origin());
        if !same_origin {
            let target = attempt.url().origin().ascii_serialization();
            attempt.error(format!(
                "refused a redirect to another origin ({target}); set the endpoint to its final URL"
            ))
        } else if attempt.previous().len() > MAX_REDIRECTS {
            attempt.error("too many redirects")
        } else {
            attempt.follow()
        }
    })
}
