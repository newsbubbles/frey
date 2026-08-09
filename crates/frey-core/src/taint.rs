//! Information-flow labels carried in the type system.
//!
//! Every value that reaches Frey from outside — a tool result, a fetched page, an MCP server's
//! description, a peer agent's reply — is [`Tainted`]. The label is two independent lattice
//! positions:
//!
//! * **integrity** — [`High`] means operator-authored; [`Low`] means someone else wrote it.
//! * **confidentiality** — [`Public`] means releasable; [`Secret`] means it came from a
//!   secret-scoped capability.
//!
//! Combining values takes the *meet* of integrity and the *join* of confidentiality, so a mixture
//! is never more trusted or less confidential than its worst component. Moving in the safe
//! direction is free; moving in the unsafe direction requires [`Tainted::endorse`] or
//! [`Tainted::declassify`], both of which are `#[track_caller]` and write to the audit trail.
//!
//! The type parameters default to the safe position, so `Tainted<String>` is untrusted and public
//! and a tool author never has to name a label to get the right behaviour.
//!
//! ```
//! use frey_core::taint::{Tainted, Validated};
//!
//! // A tool returns plain data; the framework labels it at the boundary.
//! let page: Tainted<String> = Tainted::from_tool("http_get", "<html>hello</html>".to_string());
//!
//! // Reading it is fine. Acting on it is not, until something trusted vouches for it.
//! assert!(page.peek().contains("hello"));
//!
//! // A parser is the honest endorser: narrowing the type *is* the check.
//! struct NonEmpty;
//! impl Validated<String> for NonEmpty {
//!     type Output = String;
//!     type Error = &'static str;
//!     const NAME: &'static str = "NonEmpty";
//!     fn validate(raw: String) -> Result<String, &'static str> {
//!         if raw.is_empty() { Err("empty") } else { Ok(raw) }
//!     }
//! }
//!
//! let trusted = page.validate::<NonEmpty>().expect("non-empty");
//! assert!(trusted.into_inner().contains("hello"));
//! ```

//! # What does not compile
//!
//! The permitted flows are pleasant to write; the forbidden ones are rejected by the compiler,
//! which is the entire argument for carrying labels in the type system rather than checking them at
//! runtime. Both cases below are compiled as part of the test suite and must fail.
//!
//! Untrusted data cannot simply be taken out and acted on:
//!
//! ```compile_fail
//! use frey_core::taint::{Tainted, Untrusted};
//!
//! let page: Untrusted<String> = Tainted::from_tool("http_get", "rm -rf /".to_string());
//! let _raw: String = page.into_inner();
//! ```
//!
//! And it cannot reach a sink that declared it needs trusted input, so the endorsement can never
//! simply be forgotten:
//!
//! ```compile_fail
//! use frey_core::taint::{Tainted, Trusted, Untrusted};
//!
//! fn execute(_command: Trusted<String>) {}
//!
//! let from_page: Untrusted<String> = Tainted::from_tool("http_get", "curl evil.test".to_string());
//! execute(from_page);
//! ```
//!
//! The permitted path is to validate, which narrows the type and raises integrity together:
//!
//! ```
//! use frey_core::taint::{Tainted, Untrusted, Validated};
//!
//! struct NonEmpty;
//! impl Validated<String> for NonEmpty {
//!     type Output = String;
//!     type Error = &'static str;
//!     const NAME: &'static str = "NonEmpty";
//!     fn validate(raw: String) -> Result<String, &'static str> {
//!         if raw.is_empty() { Err("empty") } else { Ok(raw) }
//!     }
//! }
//!
//! fn execute(command: String) -> usize { command.len() }
//!
//! let from_page: Untrusted<String> = Tainted::from_tool("http_get", "ls".to_string());
//! let checked = from_page.validate::<NonEmpty>().expect("non-empty");
//! assert_eq!(execute(checked.into_inner()), 2);
//! ```

use std::fmt;
use std::marker::PhantomData;

use smol_str::SmolStr;

use crate::audit::{self, AuditEvent, CallSite, Declassification, Endorsement};

mod sealed {
    pub trait Sealed {}
}

// ---------------------------------------------------------------------------------------------
// Lattice
// ---------------------------------------------------------------------------------------------

/// Runtime view of an integrity position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum IntegrityLevel {
    /// Someone other than the operator wrote this.
    Low,
    /// Operator-authored, or vouched for by something trusted.
    High,
}

/// Runtime view of a confidentiality position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ConfidentialityLevel {
    /// Releasable.
    Public,
    /// Came from a secret-scoped capability.
    Secret,
}

/// Type-level integrity position.
pub trait Integrity: sealed::Sealed + 'static {
    /// The runtime value of this position.
    const LEVEL: IntegrityLevel;
}

/// Type-level confidentiality position.
pub trait Confidentiality: sealed::Sealed + 'static {
    /// The runtime value of this position.
    const LEVEL: ConfidentialityLevel;
}

/// Operator-authored: system prompts, configuration, code.
#[derive(Debug, Clone, Copy)]
pub struct High;
/// Anything Frey did not write itself.
#[derive(Debug, Clone, Copy)]
pub struct Low;
/// Releasable.
#[derive(Debug, Clone, Copy)]
pub struct Public;
/// Came from a secret-scoped capability.
#[derive(Debug, Clone, Copy)]
pub struct Secret;

impl sealed::Sealed for High {}
impl sealed::Sealed for Low {}
impl sealed::Sealed for Public {}
impl sealed::Sealed for Secret {}

impl Integrity for High {
    const LEVEL: IntegrityLevel = IntegrityLevel::High;
}
impl Integrity for Low {
    const LEVEL: IntegrityLevel = IntegrityLevel::Low;
}
impl Confidentiality for Public {
    const LEVEL: ConfidentialityLevel = ConfidentialityLevel::Public;
}
impl Confidentiality for Secret {
    const LEVEL: ConfidentialityLevel = ConfidentialityLevel::Secret;
}

/// Greatest lower bound of two integrity positions: any `Low` input makes the result `Low`.
pub trait Meet<Rhs> {
    /// The combined position.
    type Output: Integrity;
}

impl Meet<High> for High {
    type Output = High;
}
impl Meet<Low> for High {
    type Output = Low;
}
impl Meet<High> for Low {
    type Output = Low;
}
impl Meet<Low> for Low {
    type Output = Low;
}

/// Least upper bound of two confidentiality positions: any `Secret` input makes the result `Secret`.
pub trait Join<Rhs> {
    /// The combined position.
    type Output: Confidentiality;
}

impl Join<Public> for Public {
    type Output = Public;
}
impl Join<Secret> for Public {
    type Output = Secret;
}
impl Join<Public> for Secret {
    type Output = Secret;
}
impl Join<Secret> for Secret {
    type Output = Secret;
}

// ---------------------------------------------------------------------------------------------
// Provenance
// ---------------------------------------------------------------------------------------------

/// Where a value came from, and what it passed through. The *type* carries the lattice position;
/// `Provenance` carries the story, so that a policy violation can name the file, tool, or peer that
/// introduced the offending data.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Provenance {
    /// The immediate source, e.g. `tool:http_get` or `mcp:github/list_issues`.
    pub origin: SmolStr,
    /// Everything the value has flowed through since, most recent last.
    pub via: Vec<SmolStr>,
}

impl Provenance {
    /// A value straight from `origin`.
    pub fn new(origin: impl Into<SmolStr>) -> Self {
        Self { origin: origin.into(), via: Vec::new() }
    }

    /// Note that the value passed through another stage.
    #[must_use]
    pub fn through(mut self, stage: impl Into<SmolStr>) -> Self {
        self.via.push(stage.into());
        self
    }

    /// Merge two provenances, keeping both stories.
    #[must_use]
    pub fn merge(mut self, other: Self) -> Self {
        self.via.push(other.origin);
        self.via.extend(other.via);
        self
    }

    /// A one-line summary suitable for an audit record or an error message.
    #[must_use]
    pub fn summary(&self) -> SmolStr {
        if self.via.is_empty() {
            self.origin.clone()
        } else {
            SmolStr::from(format!("{} -> {}", self.origin, self.via.join(" -> ")))
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Tainted
// ---------------------------------------------------------------------------------------------

/// A value carrying an information-flow label.
///
/// The defaults (`Low`, `Public`) are the safe position, so `Tainted<T>` means "untrusted, not
/// secret" — which is what almost everything arriving from outside actually is.
pub struct Tainted<T, I = Low, C = Public> {
    value: T,
    prov: Provenance,
    _label: PhantomData<fn() -> (I, C)>,
}

/// Untrusted and releasable. The common case.
pub type Untrusted<T> = Tainted<T, Low, Public>;
/// Operator-authored and releasable.
pub type Trusted<T> = Tainted<T, High, Public>;
/// Untrusted and confidential.
pub type UntrustedSecret<T> = Tainted<T, Low, Secret>;
/// Operator-authored and confidential.
pub type TrustedSecret<T> = Tainted<T, High, Secret>;

impl<T, I, C> Tainted<T, I, C> {
    /// Wrap a value with an explicit provenance.
    pub fn with_provenance(value: T, prov: Provenance) -> Self {
        Self { value, prov, _label: PhantomData }
    }

    /// Look at the value without taking ownership. Reading is always allowed; it is *acting* on
    /// low-integrity data that the type system restricts.
    pub fn peek(&self) -> &T {
        &self.value
    }

    /// Where this value came from.
    pub fn provenance(&self) -> &Provenance {
        &self.prov
    }

    /// Transform the value, keeping the label and provenance.
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Tainted<U, I, C> {
        Tainted { value: f(self.value), prov: self.prov, _label: PhantomData }
    }

    /// Note that the value passed through another stage.
    #[must_use]
    pub fn through(mut self, stage: impl Into<SmolStr>) -> Self {
        self.prov = self.prov.through(stage);
        self
    }

    /// Discard the value, keeping only its label position and story. Useful when a stage consumed
    /// tainted input and its output must inherit the taint.
    pub fn into_provenance(self) -> Provenance {
        self.prov
    }
}

impl<T> Tainted<T, Low, Public> {
    /// Label a value that came back from a tool.
    pub fn from_tool(tool: &str, value: T) -> Self {
        Self::with_provenance(value, Provenance::new(format!("tool:{tool}")))
    }

    /// Label a value that came from a remote peer agent.
    pub fn from_peer(peer: &str, value: T) -> Self {
        Self::with_provenance(value, Provenance::new(format!("peer:{peer}")))
    }

    /// Label text the model itself produced.
    pub fn from_model(model: &str, value: T) -> Self {
        Self::with_provenance(value, Provenance::new(format!("model:{model}")))
    }
}

impl<T> Tainted<T, High, Public> {
    /// Label a value the operator wrote: a system prompt, a config field, a literal in the source.
    pub fn from_operator(what: &str, value: T) -> Self {
        Self::with_provenance(value, Provenance::new(format!("operator:{what}")))
    }

    /// Take the value out. Only available at high integrity and public confidentiality, which is
    /// exactly the precondition every side-effecting sink needs.
    pub fn into_inner(self) -> T {
        self.value
    }
}

impl<T, C> Tainted<T, Low, C>
where
    C: Confidentiality,
{
    /// Raise integrity from `Low` to `High`.
    ///
    /// This is one of only two operations that move against the lattice. Every call site is
    /// findable with `grep endorse`, and every invocation writes an [`AuditEvent::Endorsed`]
    /// naming the caller's file and line.
    #[track_caller]
    #[must_use = "endorsing and discarding the result defeats the purpose"]
    pub fn endorse(self, reason: Endorsement) -> Tainted<T, High, C> {
        audit::record(AuditEvent::Endorsed {
            site: CallSite::here(),
            reason,
            origin: self.prov.summary(),
        });
        Tainted { value: self.value, prov: self.prov, _label: PhantomData }
    }
}

impl<T, I> Tainted<T, I, Secret>
where
    I: Integrity,
{
    /// Lower confidentiality from `Secret` to `Public`.
    ///
    /// The other operation that moves against the lattice, and the one that decides whether a
    /// secret can leave the process. Audited identically to [`Tainted::endorse`].
    #[track_caller]
    #[must_use = "declassifying and discarding the result defeats the purpose"]
    pub fn declassify(self, reason: Declassification) -> Tainted<T, I, Public> {
        audit::record(AuditEvent::Declassified {
            site: CallSite::here(),
            reason,
            origin: self.prov.summary(),
        });
        Tainted { value: self.value, prov: self.prov, _label: PhantomData }
    }
}

impl<T, I, C> Tainted<T, I, C>
where
    I: Integrity,
    C: Confidentiality,
{
    /// The runtime label, for policy checks and error messages.
    pub fn label(&self) -> (IntegrityLevel, ConfidentialityLevel) {
        (I::LEVEL, C::LEVEL)
    }

    /// Combine with another labelled value. Integrity meets, confidentiality joins.
    pub fn zip<U, I2, C2>(
        self,
        other: Tainted<U, I2, C2>,
    ) -> Tainted<(T, U), <I as Meet<I2>>::Output, <C as Join<C2>>::Output>
    where
        I: Meet<I2>,
        C: Join<C2>,
    {
        Tainted {
            value: (self.value, other.value),
            prov: self.prov.merge(other.prov),
            _label: PhantomData,
        }
    }

    /// Move to a strictly safer position: integrity may only fall, confidentiality may only rise.
    /// Always allowed, never audited, because it cannot create a security problem.
    pub fn downgrade<I2, C2>(self) -> Tainted<T, I2, C2>
    where
        I2: Integrity,
        C2: Confidentiality,
        I: Meet<I2, Output = I2>,
        C: Join<C2, Output = C2>,
    {
        Tainted { value: self.value, prov: self.prov, _label: PhantomData }
    }

    /// Run a validator. Success narrows the type *and* raises integrity, because the validator —
    /// not the model — decided the shape. This is the endorsement path that should be reached for
    /// first, and the only one that needs no human in the loop.
    ///
    /// # Errors
    /// Returns the validator's error, with the value's provenance attached so a failure can be
    /// traced back to whatever produced the bad data.
    #[track_caller]
    pub fn validate<V>(self) -> Result<Tainted<V::Output, High, C>, ValidationFailed<V::Error>>
    where
        V: Validated<T>,
    {
        match V::validate(self.value) {
            Ok(out) => {
                audit::record(AuditEvent::Endorsed {
                    site: CallSite::here(),
                    reason: Endorsement::Parsed { validator: V::NAME },
                    origin: self.prov.summary(),
                });
                Ok(Tainted {
                    value: out,
                    prov: self.prov.through(format!("validate:{}", V::NAME)),
                    _label: PhantomData,
                })
            }
            Err(error) => {
                Err(ValidationFailed { validator: V::NAME, provenance: self.prov, error })
            }
        }
    }
}

/// A parser that is trusted to decide whether raw input has an acceptable shape.
pub trait Validated<Raw> {
    /// The narrowed type produced on success.
    type Output;
    /// Why validation failed.
    type Error;
    /// Identifies this validator in the audit trail.
    const NAME: &'static str;

    /// Attempt to narrow `raw`.
    ///
    /// # Errors
    /// Returns `Self::Error` when the input does not have the required shape.
    fn validate(raw: Raw) -> Result<Self::Output, Self::Error>;
}

/// A validator rejected the value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationFailed<E> {
    /// Which validator rejected it.
    pub validator: &'static str,
    /// Where the bad value came from.
    pub provenance: Provenance,
    /// The validator's own error.
    pub error: E,
}

impl<E: fmt::Display> fmt::Display for ValidationFailed<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} rejected a value from {}: {}",
            self.validator,
            self.provenance.summary(),
            self.error
        )
    }
}

impl<E: fmt::Debug + fmt::Display> std::error::Error for ValidationFailed<E> {}

// `Tainted` deliberately does not implement `Deref`, `Display`, `Serialize`, or `Into<T>`: every
// route to the inner value should be a decision someone made on purpose.
impl<T: fmt::Debug, I: Integrity, C: Confidentiality> fmt::Debug for Tainted<T, I, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tainted")
            .field("integrity", &I::LEVEL)
            .field("confidentiality", &C::LEVEL)
            .field("origin", &self.prov.summary())
            .field("value", &self.value)
            .finish()
    }
}

impl<T: Clone, I, C> Clone for Tainted<T, I, C> {
    fn clone(&self) -> Self {
        Self { value: self.value.clone(), prov: self.prov.clone(), _label: PhantomData }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::audit::{MemorySink, scoped_sink};

    struct NonEmpty;
    impl Validated<String> for NonEmpty {
        type Output = String;
        type Error = &'static str;
        const NAME: &'static str = "NonEmpty";
        fn validate(raw: String) -> Result<String, &'static str> {
            if raw.trim().is_empty() { Err("blank") } else { Ok(raw) }
        }
    }

    #[test]
    fn defaults_are_the_safe_position() {
        let t: Tainted<&str> = Tainted::from_tool("fs_read", "hi");
        assert_eq!(t.label(), (IntegrityLevel::Low, ConfidentialityLevel::Public));
    }

    #[test]
    fn zip_meets_integrity_and_joins_confidentiality() {
        let trusted: Tainted<u8, High, Public> = Tainted::from_operator("config", 1);
        let secret: Tainted<u8, High, Secret> =
            Tainted::with_provenance(2, Provenance::new("secret:token"));
        let combined = trusted.zip(secret);
        // High meet High = High; Public join Secret = Secret.
        assert_eq!(combined.label(), (IntegrityLevel::High, ConfidentialityLevel::Secret));

        let untrusted: Tainted<u8> = Tainted::from_tool("http_get", 3);
        let mixed = combined.zip(untrusted);
        // High meet Low = Low; Secret join Public = Secret.
        assert_eq!(mixed.label(), (IntegrityLevel::Low, ConfidentialityLevel::Secret));
    }

    #[test]
    fn provenance_survives_combination() {
        let a: Tainted<u8> = Tainted::from_tool("fs_read", 1);
        let b: Tainted<u8> = Tainted::from_peer("planner", 2);
        let combined = a.zip(b);
        let summary = combined.provenance().summary();
        assert!(summary.contains("tool:fs_read"), "got {summary}");
        assert!(summary.contains("peer:planner"), "got {summary}");
    }

    #[test]
    fn endorse_writes_an_audit_record_pointing_at_the_caller() {
        let sink = Arc::new(MemorySink::new());
        let _guard = scoped_sink(sink.clone());

        let page: Tainted<String> = Tainted::from_tool("http_get", "body".into());
        let endorsed = page.endorse(Endorsement::OperatorAsserted("fixture"));
        assert_eq!(endorsed.label().0, IntegrityLevel::High);

        let events = sink.events();
        assert_eq!(events.len(), 1);
        match &events[0] {
            AuditEvent::Endorsed { site, reason, origin } => {
                assert!(site.file.ends_with("taint.rs"), "got {}", site.file);
                assert_eq!(reason, &Endorsement::OperatorAsserted("fixture"));
                assert_eq!(origin.as_str(), "tool:http_get");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn declassify_writes_an_audit_record() {
        let sink = Arc::new(MemorySink::new());
        let _guard = scoped_sink(sink.clone());

        let token: Tainted<String, High, Secret> =
            Tainted::with_provenance("hunter2".into(), Provenance::new("secret:api_key"));
        let released = token.declassify(Declassification::Redacted { redactor: "last4" });
        assert_eq!(released.label().1, ConfidentialityLevel::Public);
        assert!(matches!(sink.events().as_slice(), [AuditEvent::Declassified { .. }]));
    }

    #[test]
    fn validation_endorses_on_success_and_records_the_validator() {
        let sink = Arc::new(MemorySink::new());
        let _guard = scoped_sink(sink.clone());

        let raw: Tainted<String> = Tainted::from_tool("fs_read", "contents".into());
        let ok = raw.validate::<NonEmpty>().expect("non-empty input validates");
        assert_eq!(ok.label().0, IntegrityLevel::High);
        assert_eq!(ok.into_inner(), "contents");

        match sink.events().as_slice() {
            [AuditEvent::Endorsed { reason: Endorsement::Parsed { validator }, .. }] => {
                assert_eq!(*validator, "NonEmpty");
            }
            other => panic!("unexpected events: {other:?}"),
        }
    }

    #[test]
    fn validation_failure_carries_provenance_and_writes_no_audit_record() {
        let sink = Arc::new(MemorySink::new());
        let _guard = scoped_sink(sink.clone());

        let raw: Tainted<String> = Tainted::from_peer("scraper", "   ".into());
        let err = raw.validate::<NonEmpty>().expect_err("blank input is rejected");
        assert_eq!(err.validator, "NonEmpty");
        assert_eq!(err.provenance.origin.as_str(), "peer:scraper");
        assert!(sink.is_empty(), "a failed validation must not look like an endorsement");
    }

    #[test]
    fn downgrade_is_free_and_unaudited() {
        let sink = Arc::new(MemorySink::new());
        let _guard = scoped_sink(sink.clone());

        let trusted: Tainted<u8, High, Public> = Tainted::from_operator("literal", 7);
        let weaker: Tainted<u8, Low, Secret> = trusted.downgrade();
        assert_eq!(weaker.label(), (IntegrityLevel::Low, ConfidentialityLevel::Secret));
        assert!(sink.is_empty(), "moving to a safer position is not a security event");
    }

    #[test]
    fn map_preserves_the_label() {
        let t: Tainted<String> = Tainted::from_tool("fs_read", "abc".into());
        let n = t.map(|s| s.len());
        assert_eq!(n.label(), (IntegrityLevel::Low, ConfidentialityLevel::Public));
        assert_eq!(*n.peek(), 3);
    }

    #[test]
    fn debug_shows_the_label_and_origin() {
        let t: Tainted<&str> = Tainted::from_tool("shell", "output");
        let rendered = format!("{t:?}");
        assert!(rendered.contains("Low"), "got {rendered}");
        assert!(rendered.contains("tool:shell"), "got {rendered}");
    }
}
