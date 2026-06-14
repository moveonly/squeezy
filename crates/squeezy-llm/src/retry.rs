mod classifier;
mod policy;
mod request;
mod stream;

pub use policy::{RetryPolicy, idle_timeout};
pub use request::{send_with_auth_retry, send_with_retry};
pub use stream::with_stream_retry;

#[cfg(test)]
pub(crate) use classifier::is_terminal_quota_error;
#[cfg(test)]
pub(crate) use policy::{JITTER_FRACTION, apply_jitter, backoff, capped_backoff, jitter_sample};
#[cfg(test)]
pub(crate) use request::parse_retry_after;
#[cfg(test)]
pub(crate) use stream::split_delta_prefix;

#[cfg(test)]
#[path = "retry_tests.rs"]
mod tests;
