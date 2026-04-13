//! HTTP utilities shared across all API clients.
//!
//! Provides:
//!   • `retry_get`     — GET with up to 3 retries + exponential backoff
//!   • `retry_builder` — same, accepts a closure that builds the RequestBuilder
//!
//! Retry policy:
//!   - Network / timeout errors  → always retry
//!   - HTTP 429 (rate limit)     → retry with backoff
//!   - HTTP 5xx (server error)   → retry
//!   - HTTP 4xx (client error)   → no retry (problem is in the request itself)
//!   - HTTP 2xx                  → return immediately

use anyhow::{anyhow, Result};

const MAX_RETRIES: u32 = 3;
const BASE_DELAY_MS: u64 = 400;

/// GET `url` with automatic retry on transient failures.
pub async fn retry_get(client: &reqwest::Client, url: &str) -> Result<reqwest::Response> {
    retry_builder(|| client.get(url)).await
}

/// Execute the request returned by `make()` with automatic retry.
///
/// `make` is called fresh for every attempt because [`reqwest::RequestBuilder`]
/// is not `Clone`.
pub async fn retry_builder<F>(make: F) -> Result<reqwest::Response>
where
    F: Fn() -> reqwest::RequestBuilder,
{
    let mut last_err = anyhow!("no attempts made");

    for attempt in 0..MAX_RETRIES {
        if attempt > 0 {
            let delay_ms = BASE_DELAY_MS * (1u64 << attempt); // 800ms, 1600ms
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        }

        match make().send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    return Ok(resp);
                }
                // Retryable server-side errors
                if status.as_u16() == 429 || status.is_server_error() {
                    last_err = anyhow!("HTTP {}", status);
                    continue;
                }
                // 4xx client errors (except 429) — return so the caller can
                // produce a meaningful error message with the body.
                return Ok(resp);
            }
            Err(e) => {
                last_err = e.into();
            }
        }
    }

    Err(last_err)
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_delay_grows_exponentially() {
        // attempt 0 → no delay
        // attempt 1 → BASE_DELAY_MS * 2 = 800ms
        // attempt 2 → BASE_DELAY_MS * 4 = 1600ms
        assert_eq!(BASE_DELAY_MS * (1u64 << 1), 800);
        assert_eq!(BASE_DELAY_MS * (1u64 << 2), 1600);
    }

    #[test]
    fn max_retries_is_reasonable() {
        // 3 retries at most — we don't want to block the UI for too long
        assert!(MAX_RETRIES <= 3);
        assert!(MAX_RETRIES >= 1);
    }
}
