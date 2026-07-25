//! `ai-memory generate-auth-token` — print a random bearer token.
//!
//! Operator workflow:
//!   $ ai-memory generate-auth-token >> docker/.env.production
//!   $ $EDITOR docker/.env.production    # prefix with AI_MEMORY_AUTH_TOKEN=
//!
//! The token is read from `AI_MEMORY_AUTH_TOKEN` (or
//! `[auth].bearer_token` in config.toml) at server startup and
//! validated on every HTTP request via the middleware in `auth.rs`.

use anyhow::Result;

use crate::cli::GenerateAuthTokenArgs;
use crate::config::Config;
use ai_memory_mcp::auth::generate_token_hex;

/// Run the `generate-auth-token` subcommand.
///
/// # Errors
/// Propagates failures from the OS RNG.
pub fn run(_config: &Config, args: GenerateAuthTokenArgs) -> Result<()> {
    // getrandom::Error in 0.3 doesn't implement std::error::Error, so
    // anyhow's Context trait doesn't apply. Map manually.
    let token = generate_token_hex(args.bytes)
        .map_err(|e| anyhow::anyhow!("generating random token: {e}"))?;
    println!("{token}");
    Ok(())
}
