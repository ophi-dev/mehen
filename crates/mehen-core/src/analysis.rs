// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::backend::AnalysisBackend;
use crate::diagnostic::ParseDiagnostic;
use crate::language::Language;
use crate::metric_key::MetricKey;
use crate::space::MetricSpace;
use crate::span::SourceSpan;

/// A metric value carried in `MetricSet`. Float and integer are kept distinct
/// so reports preserve their natural shape (parity tolerance also depends on
/// it — integers are bit-exact, floats have per-metric tolerance).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MetricValue {
    Int(i64),
    Float(f64),
}

impl MetricValue {
    pub fn as_f64(&self) -> f64 {
        match self {
            MetricValue::Int(i) => *i as f64,
            MetricValue::Float(f) => *f,
        }
    }
}

impl From<i64> for MetricValue {
    fn from(v: i64) -> Self {
        MetricValue::Int(v)
    }
}

impl From<u32> for MetricValue {
    fn from(v: u32) -> Self {
        MetricValue::Int(i64::from(v))
    }
}

impl From<u64> for MetricValue {
    fn from(v: u64) -> Self {
        // Saturate large counts into a `Float` so values that exceed
        // `i64::MAX` are still representable instead of silently wrapping
        // to a negative integer.
        if v <= i64::MAX as u64 {
            MetricValue::Int(v as i64)
        } else {
            MetricValue::Float(v as f64)
        }
    }
}

impl From<usize> for MetricValue {
    fn from(v: usize) -> Self {
        if v <= i64::MAX as usize {
            MetricValue::Int(v as i64)
        } else {
            MetricValue::Float(v as f64)
        }
    }
}

impl From<f64> for MetricValue {
    fn from(v: f64) -> Self {
        MetricValue::Float(v)
    }
}

/// The metric values published by an analyzer for a single space.
///
/// Stored ordered (`BTreeMap`) so JSON snapshots are deterministic without
/// requiring a separate sort step at render time.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MetricSet(BTreeMap<MetricKey, MetricValue>);

impl MetricSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, key: impl Into<MetricKey>, value: impl Into<MetricValue>) {
        self.0.insert(key.into(), value.into());
    }

    pub fn get(&self, key: &MetricKey) -> Option<MetricValue> {
        self.0.get(key).copied()
    }

    pub fn remove(&mut self, key: &MetricKey) -> Option<MetricValue> {
        self.0.remove(key)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&MetricKey, &MetricValue)> {
        self.0.iter()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// A single fact contributed by an analyzer toward a metric.
///
/// Per the rewrite plan §5.4: this is the explainability primitive. It lets
/// `mehen diff` answer "why did `cognitive` move +3 here" with a span and a
/// reason code. Not all analyzers need to produce contributions in 1.0 — the
/// shape exists so they can be added per metric without changing the report
/// schema.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MetricContribution {
    pub metric: MetricKey,
    pub span: SourceSpan,
    pub amount: f64,
    pub reason: ContributionReason,
}

/// Optional, deterministically ordered sink for analyzer contribution evidence.
///
/// Analyzers can construct this directly from
/// [`crate::AnalysisConfig::emit_contributions`]. When disabled, [`record`](Self::record)
/// is a cheap no-op; when enabled, [`finish`](Self::finish) returns entries in
/// source order so JSON and snapshot output remain stable across parser walk
/// implementations and platforms.
#[derive(Debug)]
pub struct ContributionCollector {
    enabled: bool,
    entries: Vec<MetricContribution>,
}

impl ContributionCollector {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            entries: Vec::new(),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn record(
        &mut self,
        metric: impl Into<MetricKey>,
        span: SourceSpan,
        amount: f64,
        reason: impl Into<String>,
    ) {
        if !self.enabled {
            return;
        }
        self.entries.push(MetricContribution {
            metric: metric.into(),
            span,
            amount,
            reason: ContributionReason::new(reason),
        });
    }

    pub fn finish(mut self) -> Vec<MetricContribution> {
        self.entries.sort_by(|a, b| {
            (
                a.span.start_byte,
                a.span.end_byte,
                &a.metric,
                a.reason.as_str(),
            )
                .cmp(&(
                    b.span.start_byte,
                    b.span.end_byte,
                    &b.metric,
                    b.reason.as_str(),
                ))
                .then_with(|| a.amount.total_cmp(&b.amount))
        });
        self.entries
    }
}

/// A namespaced reason code attached to a [`MetricContribution`].
///
/// Stored as a string so language crates can publish their own reason codes
/// (`python.match_case`, `typescript.decorator_stack`,
/// `markdown.heading_skip`) without coordinating an enum across crates.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ContributionReason(pub String);

impl ContributionReason {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The canonical owned result returned by every language analyzer.
///
/// `LanguageAnalysis` and everything inside it must be `'static` and `Send` —
/// no parser-arena borrows leak across the API boundary. This is the
/// invariant that keeps Oxc's bumpalo, Mago's Bump, and Ruff's text arenas
/// confined to the analyzer crate.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LanguageAnalysis {
    pub language: Language,
    pub backend: AnalysisBackend,
    pub diagnostics: Vec<ParseDiagnostic>,
    pub root: MetricSpace,
    pub contributions: Vec<MetricContribution>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send_static<T: Send + 'static>() {}

    #[test]
    fn language_analysis_is_send_static() {
        // Compile-time check that the analyzer output never borrows from a
        // parser arena. If a future field violates this, the build breaks
        // and forces an explicit decision rather than a silent regression.
        assert_send_static::<LanguageAnalysis>();
    }

    #[test]
    fn metric_set_is_ordered() {
        let mut set = MetricSet::new();
        set.insert("z", 1u64);
        set.insert("a", 2u64);
        let keys: Vec<&str> = set.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["a", "z"]);
    }

    #[test]
    fn contribution_collector_is_gated_and_deterministic() {
        let later = SourceSpan::new(20, 24, 3, 3);
        let earlier = SourceSpan::new(2, 6, 1, 1);
        let mut collector = ContributionCollector::new(true);
        collector.record("risk", later, 2.0, "risk.write");
        collector.record("risk", earlier, 8.0, "risk.drop");
        let entries = collector.finish();
        assert_eq!(entries[0].span, earlier);
        assert_eq!(entries[1].span, later);

        let mut disabled = ContributionCollector::new(false);
        disabled.record("risk", earlier, 8.0, "risk.drop");
        assert!(disabled.finish().is_empty());
    }
}
