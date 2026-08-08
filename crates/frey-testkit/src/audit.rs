//! Asserting on the audit trail.
//!
//! Security properties are only real if a test can see them. [`CapturedAudit`] installs a
//! thread-local sink for the duration of a test, so parallel tests never see each other's events
//! and the process-global sink is left alone.

use std::sync::Arc;

use frey_core::audit::{
    AuditEvent, Declassification, Endorsement, MemorySink, SinkGuard, scoped_sink,
};

/// Captures audit events on the current thread until dropped.
///
/// ```
/// use frey_testkit::audit::CapturedAudit;
/// use frey_core::audit::Endorsement;
/// use frey_core::taint::{Tainted, Untrusted};
///
/// let audit = CapturedAudit::install();
/// let page: Untrusted<String> = Tainted::from_tool("http_get", "body".into());
/// let _ = page.endorse(Endorsement::OperatorAsserted("test fixture"));
///
/// assert_eq!(
///     audit.endorsements(),
///     vec![Endorsement::OperatorAsserted("test fixture")],
/// );
/// ```
///
/// Use [`CapturedAudit::assert_endorsed_at`] to pin the *call site* as well as the reason — the
/// file and line are what make an audit trail answerable rather than merely present.
#[derive(Debug)]
pub struct CapturedAudit {
    sink: Arc<MemorySink>,
    _guard: SinkGuard,
}

impl CapturedAudit {
    /// Start capturing on this thread.
    #[must_use]
    pub fn install() -> Self {
        let sink = Arc::new(MemorySink::new());
        let guard = scoped_sink(sink.clone());
        Self { sink, _guard: guard }
    }

    /// Every event, in order.
    #[must_use]
    pub fn events(&self) -> Vec<AuditEvent> {
        self.sink.events()
    }

    /// Whether anything at all was recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sink.is_empty()
    }

    /// Every integrity-raising event.
    #[must_use]
    pub fn endorsements(&self) -> Vec<Endorsement> {
        self.events()
            .into_iter()
            .filter_map(|e| match e {
                AuditEvent::Endorsed { reason, .. } => Some(reason),
                _ => None,
            })
            .collect()
    }

    /// Every confidentiality-lowering event.
    #[must_use]
    pub fn declassifications(&self) -> Vec<Declassification> {
        self.events()
            .into_iter()
            .filter_map(|e| match e {
                AuditEvent::Declassified { reason, .. } => Some(reason),
                _ => None,
            })
            .collect()
    }

    /// Assert that some endorsement was recorded from a file whose name contains `fragment`.
    ///
    /// The call site is the whole point of the audit trail: "untrusted data became trusted
    /// *somewhere*" is not an answer an auditor can use.
    ///
    /// # Panics
    /// If no endorsement was recorded from a matching file.
    pub fn assert_endorsed_at(&self, fragment: &str) {
        let sites: Vec<String> = self
            .events()
            .iter()
            .filter_map(|e| match e {
                AuditEvent::Endorsed { site, .. } => Some(site.to_string()),
                _ => None,
            })
            .collect();
        assert!(
            sites.iter().any(|s| s.contains(fragment)),
            "expected an endorsement from a file containing {fragment:?}, but the trail was {sites:?}"
        );
    }

    /// Assert that nothing raised integrity or lowered confidentiality.
    ///
    /// # Panics
    /// If any event was recorded.
    pub fn assert_silent(&self) {
        let events = self.events();
        assert!(events.is_empty(), "expected no security-relevant decisions, but got {events:#?}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use frey_core::audit::Endorsement;
    use frey_core::taint::{Tainted, Untrusted, Validated};

    struct AlwaysOk;
    impl Validated<String> for AlwaysOk {
        type Output = String;
        type Error = &'static str;
        const NAME: &'static str = "AlwaysOk";
        fn validate(raw: String) -> Result<String, &'static str> {
            Ok(raw)
        }
    }

    #[test]
    fn captures_validation_endorsements_with_their_validator() {
        let audit = CapturedAudit::install();
        let raw: Untrusted<String> = Tainted::from_tool("fs_read", "contents".into());
        let _ = raw.validate::<AlwaysOk>().unwrap();

        assert_eq!(audit.endorsements(), vec![Endorsement::Parsed { validator: "AlwaysOk" }]);
    }

    #[test]
    fn the_call_site_is_the_callers_file_not_freys() {
        // The property that makes the trail useful to an auditor: "untrusted data became trusted
        // somewhere in the framework" is not an answer.
        let audit = CapturedAudit::install();
        let page: Untrusted<String> = Tainted::from_tool("http_get", "body".into());
        let _endorsed = page.endorse(Endorsement::OperatorAsserted("fixture"));
        audit.assert_endorsed_at("audit.rs");
    }

    #[test]
    fn a_safe_path_records_nothing() {
        let audit = CapturedAudit::install();
        let page: Untrusted<String> = Tainted::from_tool("http_get", "body".into());
        // Reading is always allowed and is not a security decision.
        assert_eq!(page.peek().len(), 4);
        audit.assert_silent();
    }
}
