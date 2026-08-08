//! Tokens and money.
//!
//! Two rules, both load-bearing:
//!
//! 1. **Never invent a cost the provider did not report.** [`Usage::reported_cost`] is `None`
//!    unless the provider sent one. Anything Frey works out itself is a [`CostEstimate`] and is
//!    labelled as such wherever it is shown.
//! 2. **Token counts are not comparable across providers.** OpenRouter reports `prompt_tokens`
//!    using each model's *native* tokenizer, so summing tokens across models is meaningless.
//!    Cost is the only comparable unit, which is why [`UsageTotals`] keeps them apart.

use std::ops::AddAssign;

use serde_json::value::RawValue;

/// A currency. Kept explicit so a ledger can refuse to add dollars to euros.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Default,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(rename_all = "UPPERCASE")]
#[non_exhaustive]
pub enum Currency {
    /// United States dollar.
    #[default]
    Usd,
    /// Euro.
    Eur,
    /// Provider-internal credits that do not map to a currency.
    Credits,
}

/// An amount of money, in millionths of a unit, so that arithmetic is exact.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct Money {
    /// Millionths of a currency unit.
    pub micros: i64,
    /// Which currency.
    pub currency: Currency,
}

impl Money {
    /// Zero, in `currency`.
    #[must_use]
    pub fn zero(currency: Currency) -> Self {
        Self { micros: 0, currency }
    }

    /// An amount in micro-units.
    #[must_use]
    pub fn micros(micros: i64, currency: Currency) -> Self {
        Self { micros, currency }
    }

    /// An amount in US dollars, from a float. Rounds to the nearest micro-unit.
    #[must_use]
    pub fn usd(amount: f64) -> Self {
        Self { micros: (amount * 1_000_000.0).round() as i64, currency: Currency::Usd }
    }

    /// Add, if the currencies match.
    ///
    /// # Errors
    /// Returns [`CurrencyMismatch`] when the currencies differ, rather than silently producing a
    /// number that means nothing.
    pub fn checked_add(self, other: Self) -> Result<Self, CurrencyMismatch> {
        if self.currency != other.currency {
            return Err(CurrencyMismatch { left: self.currency, right: other.currency });
        }
        Ok(Self { micros: self.micros.saturating_add(other.micros), currency: self.currency })
    }
}

/// Two amounts in different currencies were combined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("cannot combine {left:?} and {right:?} without an exchange rate")]
pub struct CurrencyMismatch {
    /// The left-hand currency.
    pub left: Currency,
    /// The right-hand currency.
    pub right: Currency,
}

/// Where a cost figure came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PricingSource {
    /// The provider reported it. Trustworthy.
    Reported,
    /// Computed from a pricing table shipped with Frey. May be stale.
    LocalTable,
    /// Computed from operator-supplied prices in configuration.
    Configured,
}

impl PricingSource {
    /// Whether a figure from this source may be presented without an "estimated" qualifier.
    #[must_use]
    pub fn is_authoritative(self) -> bool {
        matches!(self, Self::Reported)
    }
}

/// A cost figure, always carrying where it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CostEstimate {
    /// The amount.
    pub amount: Money,
    /// Where the number came from.
    pub source: PricingSource,
}

/// What one model call consumed.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Usage {
    /// Fresh input tokens.
    ///
    /// **Anthropic caveat:** their `input_tokens` counts only tokens *after* the last cache
    /// breakpoint, so the total is `cache_read + cache_write + input`, not `input` alone.
    pub input: u64,
    /// Generated tokens.
    pub output: u64,
    /// Tokens served from cache.
    pub cache_read: u64,
    /// Tokens written to cache.
    pub cache_write: u64,
    /// Reasoning tokens, where the provider separates them.
    pub reasoning: u64,
    /// The cost the provider reported. `None` means the provider did not say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_cost: Option<Money>,
    /// The provider's own usage object, kept verbatim for auditing and for fields Frey does not
    /// model yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<Box<RawValue>>,
}

impl PartialEq for Usage {
    fn eq(&self, other: &Self) -> bool {
        self.input == other.input
            && self.output == other.output
            && self.cache_read == other.cache_read
            && self.cache_write == other.cache_write
            && self.reasoning == other.reasoning
            && self.reported_cost == other.reported_cost
            && self.raw.as_deref().map(RawValue::get) == other.raw.as_deref().map(RawValue::get)
    }
}

impl Eq for Usage {}

impl Usage {
    /// Every input token the request consumed, cached or not.
    #[must_use]
    pub fn total_input(&self) -> u64 {
        self.input + self.cache_read + self.cache_write
    }

    /// The fraction of input tokens served from cache, in `0.0..=1.0`.
    ///
    /// Returns `None` when there were no input tokens, rather than a misleading zero.
    #[must_use]
    pub fn cache_hit_rate(&self) -> Option<f64> {
        let total = self.total_input();
        if total == 0 {
            None
        } else {
            #[allow(clippy::cast_precision_loss)]
            Some(self.cache_read as f64 / total as f64)
        }
    }
}

/// Running totals across many calls.
///
/// Token counts are kept **per model**, because summing tokens across providers is meaningless:
/// each uses its own tokenizer. Cost is summed globally, because money is comparable.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UsageTotals {
    /// Per-model token counts, keyed by `provider:model`.
    pub by_model: std::collections::BTreeMap<String, TokenTotals>,
    /// Total reported cost. `None` if no call reported one.
    pub reported_cost: Option<Money>,
    /// How many calls reported no cost, so a UI can say "plus N unmetered calls" instead of
    /// implying the total is complete.
    pub unmetered_calls: u32,
}

/// Token counts for one model.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TokenTotals {
    /// Fresh input.
    pub input: u64,
    /// Output.
    pub output: u64,
    /// Cache reads.
    pub cache_read: u64,
    /// Cache writes.
    pub cache_write: u64,
    /// Reasoning.
    pub reasoning: u64,
    /// How many calls contributed.
    pub calls: u32,
}

impl AddAssign<&Usage> for TokenTotals {
    fn add_assign(&mut self, rhs: &Usage) {
        self.input += rhs.input;
        self.output += rhs.output;
        self.cache_read += rhs.cache_read;
        self.cache_write += rhs.cache_write;
        self.reasoning += rhs.reasoning;
        self.calls += 1;
    }
}

impl UsageTotals {
    /// Fold one call's usage in.
    ///
    /// # Errors
    /// Returns [`CurrencyMismatch`] if a call reports a currency that differs from the running
    /// total's.
    pub fn record(&mut self, model_key: &str, usage: &Usage) -> Result<(), CurrencyMismatch> {
        *self.by_model.entry(model_key.to_owned()).or_default() += usage;
        match (&mut self.reported_cost, usage.reported_cost) {
            (slot @ None, Some(cost)) => *slot = Some(cost),
            (Some(running), Some(cost)) => *running = running.checked_add(cost)?,
            (_, None) => self.unmetered_calls += 1,
        }
        Ok(())
    }

    /// Whether every call so far reported its cost. When false, the total understates spend and any
    /// UI must say so.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.unmetered_calls == 0
    }

    /// Cache hit rate across a single model's calls.
    #[must_use]
    pub fn cache_hit_rate(&self, model_key: &str) -> Option<f64> {
        let t = self.by_model.get(model_key)?;
        let total = t.input + t.cache_read + t.cache_write;
        if total == 0 {
            None
        } else {
            #[allow(clippy::cast_precision_loss)]
            Some(t.cache_read as f64 / total as f64)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: u64, cache_read: u64, cache_write: u64) -> Usage {
        Usage { input, cache_read, cache_write, output: 10, ..Usage::default() }
    }

    #[test]
    fn total_input_accounts_for_anthropics_post_breakpoint_counting() {
        // Anthropic reports 50 fresh tokens, 100_000 read from cache, 5_120 written. The naive
        // reading is "50 input tokens", which understates the request by three orders of magnitude.
        let u = usage(50, 100_000, 5_120);
        assert_eq!(u.total_input(), 105_170);
    }

    #[test]
    fn cache_hit_rate_is_absent_rather_than_zero_when_there_is_nothing_to_measure() {
        assert_eq!(Usage::default().cache_hit_rate(), None);
        let u = usage(0, 900, 100);
        assert!((u.cache_hit_rate().unwrap() - 0.9).abs() < f64::EPSILON);
    }

    #[test]
    fn a_cost_is_never_invented() {
        let u = usage(100, 0, 0);
        assert_eq!(u.reported_cost, None, "no provider figure means no figure");

        let mut totals = UsageTotals::default();
        totals.record("anthropic:claude-opus-5", &u).unwrap();
        assert_eq!(totals.reported_cost, None);
        assert_eq!(totals.unmetered_calls, 1);
        assert!(!totals.is_complete(), "a UI must be able to say the total is partial");
    }

    #[test]
    fn tokens_are_kept_per_model_because_tokenizers_differ() {
        let mut totals = UsageTotals::default();
        totals.record("anthropic:claude-opus-5", &usage(100, 0, 0)).unwrap();
        totals.record("openrouter:moonshot/kimi", &usage(100, 0, 0)).unwrap();
        assert_eq!(totals.by_model.len(), 2, "no cross-model token summing");
        assert_eq!(totals.by_model["anthropic:claude-opus-5"].calls, 1);
    }

    #[test]
    fn reported_costs_accumulate_and_currency_mixing_is_an_error() {
        let mut totals = UsageTotals::default();
        let mut a = usage(10, 0, 0);
        a.reported_cost = Some(Money::usd(0.25));
        let mut b = usage(10, 0, 0);
        b.reported_cost = Some(Money::usd(0.50));
        totals.record("openrouter:m", &a).unwrap();
        totals.record("openrouter:m", &b).unwrap();
        assert_eq!(totals.reported_cost, Some(Money::usd(0.75)));
        assert!(totals.is_complete());

        let mut c = usage(10, 0, 0);
        c.reported_cost = Some(Money { micros: 1_000, currency: Currency::Credits });
        assert!(totals.record("openrouter:m", &c).is_err(), "credits are not dollars");
    }

    #[test]
    fn estimated_costs_are_marked_as_such() {
        let estimate = CostEstimate { amount: Money::usd(1.20), source: PricingSource::LocalTable };
        assert!(!estimate.source.is_authoritative());
        assert!(PricingSource::Reported.is_authoritative());
    }

    #[test]
    fn the_providers_own_usage_object_is_kept_verbatim() {
        let raw = r#"{"cache_creation":{"ephemeral_5m_input_tokens":148,"ephemeral_1h_input_tokens":100}}"#;
        let u = Usage {
            raw: Some(RawValue::from_string(raw.to_string()).unwrap()),
            ..Usage::default()
        };
        let decoded: Usage = serde_json::from_str(&serde_json::to_string(&u).unwrap()).unwrap();
        assert_eq!(decoded.raw.as_ref().unwrap().get(), raw);
        assert_eq!(decoded, u);
    }
}
