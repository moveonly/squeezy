use super::*;
use std::fmt;
use std::sync::{Arc, Mutex};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Level, Metadata, Subscriber};

/// Minimal in-process subscriber that records the fields of every warn!
/// event emitted on the current thread. Implementing `Subscriber` directly
/// avoids pulling `tracing-subscriber` into `squeezy-core`'s dependency
/// graph for a single test.
#[derive(Default, Clone)]
struct CapturingSubscriber {
    events: Arc<Mutex<Vec<CapturedEvent>>>,
}

#[derive(Debug, Default, Clone)]
struct CapturedEvent {
    message: String,
    limit: Option<String>,
    errno: Option<i64>,
    error: Option<String>,
}

impl CapturingSubscriber {
    fn drain(&self) -> Vec<CapturedEvent> {
        std::mem::take(&mut *self.events.lock().expect("events lock poisoned"))
    }
}

impl Subscriber for CapturingSubscriber {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= &Level::WARN
    }
    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }
    fn record(&self, _: &Id, _: &Record<'_>) {}
    fn record_follows_from(&self, _: &Id, _: &Id) {}
    fn event(&self, event: &Event<'_>) {
        let mut captured = CapturedEvent::default();
        event.record(&mut FieldVisitor(&mut captured));
        self.events
            .lock()
            .expect("events lock poisoned")
            .push(captured);
    }
    fn enter(&self, _: &Id) {}
    fn exit(&self, _: &Id) {}
}

struct FieldVisitor<'a>(&'a mut CapturedEvent);

impl Visit for FieldVisitor<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        let rendered = format!("{value:?}");
        // tracing renders `&str` fields via Debug, which adds quotes; strip
        // them so the assertion can compare against the raw limit name.
        let trimmed = rendered.trim_matches('"').to_string();
        match field.name() {
            "message" => self.0.message = rendered,
            "limit" => self.0.limit = Some(trimmed),
            "errno" => self.0.errno = rendered.parse::<i64>().ok(),
            "error" => self.0.error = Some(rendered),
            _ => {}
        }
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        if field.name() == "errno" {
            self.0.errno = Some(value);
        }
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        if field.name() == "errno" {
            self.0.errno = Some(value as i64);
        }
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "limit" => self.0.limit = Some(value.to_string()),
            "error" => self.0.error = Some(value.to_string()),
            _ => {}
        }
    }
}

/// Force `setrlimit` to fail with `EINVAL` by passing a rlim where
/// `rlim_cur > rlim_max`; POSIX requires this to be rejected, so the
/// kernel returns the error without depending on a per-target resource id.
#[test]
fn setrlimit_failure_emits_warn_with_limit_and_errno() {
    let subscriber = CapturingSubscriber::default();
    let invalid = libc::rlimit {
        rlim_cur: 1,
        rlim_max: 0,
    };

    let result = tracing::subscriber::with_default(subscriber.clone(), || {
        set_rlimit_or_warn("RLIMIT_CORE", libc::RLIMIT_CORE, &invalid)
    });

    let err = result.expect_err("setrlimit with rlim_cur>rlim_max must fail with EINVAL");
    assert_eq!(err.raw_os_error(), Some(libc::EINVAL));

    let events = subscriber.drain();
    assert_eq!(events.len(), 1, "expected exactly one warn event");
    let captured = &events[0];
    assert_eq!(captured.limit.as_deref(), Some("RLIMIT_CORE"));
    assert_eq!(captured.errno, Some(libc::EINVAL as i64));
    assert!(
        captured.error.as_deref().is_some_and(|s| !s.is_empty()),
        "expected non-empty error field, got {:?}",
        captured.error
    );
    assert!(
        captured.message.contains("setrlimit"),
        "expected setrlimit in message, got {:?}",
        captured.message
    );
}

/// The happy path must not emit any warn event so production usage stays
/// quiet when hardening succeeds (the common case).
#[test]
fn setrlimit_success_emits_no_warn() {
    let subscriber = CapturingSubscriber::default();
    // Read the current RLIMIT_CORE and re-apply it; this is always
    // permitted (rlim_cur stays <= rlim_max, rlim_max unchanged).
    let mut current = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    let rc = unsafe { libc::getrlimit(libc::RLIMIT_CORE, &mut current) };
    assert_eq!(rc, 0, "getrlimit(RLIMIT_CORE) failed in test setup");

    let result = tracing::subscriber::with_default(subscriber.clone(), || {
        set_rlimit_or_warn("RLIMIT_CORE", libc::RLIMIT_CORE, &current)
    });
    result.expect("setrlimit re-applying current limits must succeed");

    assert!(
        subscriber.drain().is_empty(),
        "no warn events expected on success",
    );
}
