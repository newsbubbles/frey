//! The audit trail.
//!
//! Every security-relevant decision in Frey produces an [`AuditEvent`]. The sink is pluggable so
//! that `frey-core` stays free of logging dependencies, and so tests can capture events without
//! racing each other: a thread-local sink takes precedence over the process-global one.

use std::cell::RefCell;
use std::sync::{Arc, OnceLock};

use smol_str::SmolStr;

/// Where an event happened in the source. Captured with `#[track_caller]`, so it points at the
/// caller of `endorse`/`declassify` rather than at Frey's internals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallSite {
    /// Source file.
    pub file: &'static str,
    /// Line number.
    pub line: u32,
}

impl CallSite {
    /// Capture the caller's location.
    #[track_caller]
    #[must_use]
    pub fn here() -> Self {
        let loc = std::panic::Location::caller();
        Self { file: loc.file(), line: loc.line() }
    }
}

impl std::fmt::Display for CallSite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.file, self.line)
    }
}

/// A security-relevant event worth keeping forever.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AuditEvent {
    /// Untrusted data was raised to high integrity.
    Endorsed {
        /// Where in the source the endorsement happened.
        site: CallSite,
        /// Why it was considered safe.
        reason: Endorsement,
        /// A short description of the value's origin.
        origin: SmolStr,
    },
    /// Confidential data was released to a less confidential context.
    Declassified {
        /// Where in the source the declassification happened.
        site: CallSite,
        /// Why it was considered safe.
        reason: Declassification,
        /// A short description of the value's origin.
        origin: SmolStr,
    },
}

/// The permitted justifications for raising integrity. There is deliberately no `Because I Said So`
/// variant that carries no evidence: every arm names something an auditor can go and check.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Endorsement {
    /// A human approved the specific action. Carries the approval record's id.
    HumanApproved(SmolStr),
    /// A parser or validator narrowed the type, and the parser is the trusted arbiter.
    Parsed {
        /// Identifies which validator ran.
        validator: &'static str,
    },
    /// A deterministic policy rule allowed it.
    PolicyAllowed {
        /// Identifies which rule fired.
        rule: &'static str,
    },
    /// The operator asserted it in code. The string must explain *why*, and it is stored verbatim.
    OperatorAsserted(&'static str),
}

/// The permitted justifications for lowering confidentiality.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Declassification {
    /// A human approved the release.
    HumanApproved(SmolStr),
    /// The value was redacted or aggregated so that the secret is no longer recoverable.
    Redacted {
        /// Identifies which redactor ran.
        redactor: &'static str,
    },
    /// A deterministic policy rule allowed the release.
    PolicyAllowed {
        /// Identifies which rule fired.
        rule: &'static str,
    },
    /// The operator asserted it in code.
    OperatorAsserted(&'static str),
}

/// A destination for audit events.
pub trait AuditSink: Send + Sync {
    /// Record one event. Implementations must not panic and must not block for long.
    fn record(&self, event: AuditEvent);
}

/// A sink that throws events away. The default, so that `frey-core` is usable standalone.
#[derive(Debug, Clone, Copy, Default)]
pub struct NullSink;

impl AuditSink for NullSink {
    fn record(&self, _event: AuditEvent) {}
}

/// A sink that keeps every event in memory. Intended for tests and for `frey doctor`.
#[derive(Debug, Default)]
pub struct MemorySink {
    events: std::sync::Mutex<Vec<AuditEvent>>,
}

impl MemorySink {
    /// A new, empty sink.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every event recorded so far, in order.
    ///
    /// # Panics
    /// If a previous call panicked while holding the lock.
    #[must_use]
    pub fn events(&self) -> Vec<AuditEvent> {
        self.events.lock().expect("audit sink poisoned").clone()
    }

    /// How many events have been recorded.
    ///
    /// # Panics
    /// If a previous call panicked while holding the lock.
    #[must_use]
    pub fn len(&self) -> usize {
        self.events.lock().expect("audit sink poisoned").len()
    }

    /// Whether no events have been recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl AuditSink for MemorySink {
    fn record(&self, event: AuditEvent) {
        self.events.lock().expect("audit sink poisoned").push(event);
    }
}

static GLOBAL_SINK: OnceLock<Arc<dyn AuditSink>> = OnceLock::new();

thread_local! {
    static LOCAL_SINK: RefCell<Option<Arc<dyn AuditSink>>> = const { RefCell::new(None) };
}

/// Install the process-wide audit sink. Only the first call takes effect, which prevents a
/// compromised code path from swapping the audit trail out from under the operator.
///
/// # Errors
/// Returns the sink back if one was already installed.
pub fn set_global_sink(sink: Arc<dyn AuditSink>) -> Result<(), Arc<dyn AuditSink>> {
    GLOBAL_SINK.set(sink)
}

/// Install a sink for the current thread only, returning a guard that restores the previous one.
///
/// This exists so that tests can assert on the audit trail without racing: each test thread has its
/// own sink, and the global sink is left alone.
#[must_use]
pub fn scoped_sink(sink: Arc<dyn AuditSink>) -> SinkGuard {
    let previous = LOCAL_SINK.with(|slot| slot.borrow_mut().replace(sink));
    SinkGuard { previous }
}

/// Restores the previous thread-local sink when dropped.
pub struct SinkGuard {
    previous: Option<Arc<dyn AuditSink>>,
}

impl std::fmt::Debug for SinkGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SinkGuard").field("had_previous", &self.previous.is_some()).finish()
    }
}

impl Drop for SinkGuard {
    fn drop(&mut self) {
        let previous = self.previous.take();
        LOCAL_SINK.with(|slot| {
            *slot.borrow_mut() = previous;
        });
    }
}

/// Send an event to whichever sink is in scope.
pub(crate) fn record(event: AuditEvent) {
    let local = LOCAL_SINK.with(|slot| slot.borrow().clone());
    if let Some(sink) = local {
        sink.record(event);
        return;
    }
    if let Some(sink) = GLOBAL_SINK.get() {
        sink.record(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoped_sink_captures_and_restores() {
        let outer = Arc::new(MemorySink::new());
        let guard = scoped_sink(outer.clone());
        record(AuditEvent::Endorsed {
            site: CallSite { file: "a.rs", line: 1 },
            reason: Endorsement::OperatorAsserted("test"),
            origin: "test".into(),
        });
        assert_eq!(outer.len(), 1);

        {
            let inner = Arc::new(MemorySink::new());
            let _inner_guard = scoped_sink(inner.clone());
            record(AuditEvent::Endorsed {
                site: CallSite { file: "b.rs", line: 2 },
                reason: Endorsement::OperatorAsserted("nested"),
                origin: "test".into(),
            });
            assert_eq!(inner.len(), 1, "nested sink receives the event");
            assert_eq!(outer.len(), 1, "outer sink is not disturbed");
        }

        record(AuditEvent::Endorsed {
            site: CallSite { file: "c.rs", line: 3 },
            reason: Endorsement::OperatorAsserted("after"),
            origin: "test".into(),
        });
        assert_eq!(outer.len(), 2, "outer sink is restored after the guard drops");
        drop(guard);
    }

    #[test]
    fn call_site_points_at_the_caller() {
        let site = CallSite::here();
        assert!(site.file.ends_with("audit.rs"), "got {}", site.file);
    }
}
