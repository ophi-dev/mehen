// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Shared contribution-evidence sink for language walkers.
//!
//! Per the rewrite plan §5.4, every metric movement should be able to
//! answer "why did this metric move here" with a source span and a
//! namespaced reason code. Language walkers already own the call sites
//! where each increment happens (a decision node, an exit statement, a
//! public member declaration, …) — this type gives those call sites a
//! uniform, cheap way to attach evidence without duplicating the
//! metric-key catalogue or the reason-code format in every crate.
//!
//! Coverage policy: evidence is recorded for the *event-shaped* metric
//! families — cyclomatic, cognitive, nexit, ABC, NOM, NArgs, NPA, NPM —
//! where each contribution is a discrete syntax construct a reader can
//! look at. Per-token / per-line families (Halstead, LOC) and derived
//! aggregates (MI, WMC) are intentionally not evidenced: listing every
//! token or physical line is noise, not explanation, and WMC/MI are
//! arithmetic over already-evidenced inputs.
//!
//! Reason codes follow `<lang>.<family>[.<detail>]`, e.g.
//! `c.cyclomatic.if_statement`, `java.abc.assignment.update_expression`,
//! `powershell.nom.closure.script_block_expression`. The `detail`
//! segment is the language crate's choice (usually the AST node kind);
//! an empty detail collapses to `<lang>.<family>`.

use mehen_core::{ContributionCollector, MetricContribution, SourceSpan, keys};

/// A language-prefixed evidence sink wrapping [`ContributionCollector`].
///
/// All record methods are no-ops when the sink is disabled (the
/// `emit_contributions` flag from `AnalysisConfig`), so walkers can call
/// them unconditionally next to the corresponding stat increment.
#[derive(Debug)]
pub struct MetricEvidence {
    collector: ContributionCollector,
    lang: &'static str,
}

impl MetricEvidence {
    /// Create a sink for `lang` (the reason-code prefix, e.g. `"c"`,
    /// `"powershell"`). `enabled` normally comes from
    /// `AnalysisConfig::emit_contributions`.
    pub fn new(lang: &'static str, enabled: bool) -> Self {
        Self {
            collector: ContributionCollector::new(enabled),
            lang,
        }
    }

    /// A disabled sink — for callers that need a placeholder.
    pub fn disabled(lang: &'static str) -> Self {
        Self::new(lang, false)
    }

    pub fn is_enabled(&self) -> bool {
        self.collector.is_enabled()
    }

    /// Sort and yield the recorded contributions (source order, then
    /// metric/reason/amount — see [`ContributionCollector::finish`]).
    pub fn finish(self) -> Vec<MetricContribution> {
        self.collector.finish()
    }

    fn record(&mut self, metric: &'static str, span: SourceSpan, amount: f64, reason: String) {
        self.collector.record(metric, span, amount, reason);
    }

    fn reason(&self, family: &str, detail: &str) -> String {
        if detail.is_empty() {
            format!("{}.{family}", self.lang)
        } else {
            format!("{}.{family}.{detail}", self.lang)
        }
    }

    /// One cyclomatic decision point (`if`, `case`, `&&`, …). Amount +1.
    pub fn decision(&mut self, span: SourceSpan, detail: &str) {
        if !self.is_enabled() {
            return;
        }
        let reason = self.reason("cyclomatic", detail);
        self.record(keys::CYCLOMATIC, span, 1.0, reason);
    }

    /// One cognitive-complexity increment. `amount` is the structural
    /// delta actually applied (`nesting + 1` for nesting constructs,
    /// `1` for flat clauses and boolean-run transitions). Zero-amount
    /// events (a same-operator boolean that did not move the metric)
    /// are skipped.
    pub fn cognitive(&mut self, span: SourceSpan, amount: u32, detail: &str) {
        if !self.is_enabled() || amount == 0 {
            return;
        }
        let reason = self.reason("cognitive", detail);
        self.record(keys::COGNITIVE, span, f64::from(amount), reason);
    }

    /// One exit point (`return`, `throw`, `raise`, …). Amount +1.
    pub fn exit(&mut self, span: SourceSpan, detail: &str) {
        if !self.is_enabled() {
            return;
        }
        let reason = self.reason("nexit", detail);
        self.record(keys::NEXIT, span, 1.0, reason);
    }

    /// One ABC assignment (`A`). Amount +1.
    pub fn abc_assignment(&mut self, span: SourceSpan, detail: &str) {
        self.abc_assignments_n(span, 1, detail);
    }

    /// `count` ABC assignments recorded as one event — for multi-target
    /// assignment forms (Go's `a, b = f()`, destructuring). Zero counts
    /// are skipped.
    pub fn abc_assignments_n(&mut self, span: SourceSpan, count: u32, detail: &str) {
        if !self.is_enabled() || count == 0 {
            return;
        }
        let reason = self.reason("abc.assignment", detail);
        self.record(ABC_ASSIGNMENTS, span, f64::from(count), reason);
    }

    /// One ABC branch (`B` — calls, `goto`, object creation). Amount +1.
    pub fn abc_branch(&mut self, span: SourceSpan, detail: &str) {
        if !self.is_enabled() {
            return;
        }
        let reason = self.reason("abc.branch", detail);
        self.record(ABC_BRANCHES, span, 1.0, reason);
    }

    /// One ABC condition (`C` — comparisons, conditional clauses).
    /// Amount +1.
    pub fn abc_condition(&mut self, span: SourceSpan, detail: &str) {
        if !self.is_enabled() {
            return;
        }
        let reason = self.reason("abc.condition", detail);
        self.record(ABC_CONDITIONS, span, 1.0, reason);
    }

    /// One function declaration (NOM). Amount +1.
    pub fn function(&mut self, span: SourceSpan, detail: &str) {
        if !self.is_enabled() {
            return;
        }
        let reason = self.reason("nom.function", detail);
        self.record(NOM_FUNCTIONS, span, 1.0, reason);
    }

    /// One closure / lambda declaration (NOM). Amount +1.
    pub fn closure(&mut self, span: SourceSpan, detail: &str) {
        if !self.is_enabled() {
            return;
        }
        let reason = self.reason("nom.closure", detail);
        self.record(NOM_CLOSURES, span, 1.0, reason);
    }

    /// The declared parameter count of a function space (NArgs).
    /// Zero-argument declarations are skipped — they don't move the
    /// metric.
    pub fn function_args(&mut self, span: SourceSpan, count: u32, detail: &str) {
        if !self.is_enabled() || count == 0 {
            return;
        }
        let reason = self.reason("nargs.function", detail);
        self.record(keys::NARGS, span, f64::from(count), reason);
    }

    /// The declared parameter count of a closure space (NArgs). Zero
    /// counts are skipped.
    pub fn closure_args(&mut self, span: SourceSpan, count: u32, detail: &str) {
        if !self.is_enabled() || count == 0 {
            return;
        }
        let reason = self.reason("nargs.closure", detail);
        self.record(keys::NARGS, span, f64::from(count), reason);
    }

    /// One public attribute of a class-like container (NPA). Amount +1.
    /// Non-public members are not evidenced — the headline metric
    /// counts public members only.
    pub fn public_attribute(&mut self, span: SourceSpan, detail: &str) {
        if !self.is_enabled() {
            return;
        }
        let reason = self.reason("npa", detail);
        self.record(keys::NPA, span, 1.0, reason);
    }

    /// One public method of a class-like container (NPM). Amount +1.
    pub fn public_method(&mut self, span: SourceSpan, detail: &str) {
        if !self.is_enabled() {
            return;
        }
        let reason = self.reason("npm", detail);
        self.record(keys::NPM, span, 1.0, reason);
    }
}

// Contribution metric keys for families whose evidence attaches to a
// published sub-key rather than the root key. Kept in sync with
// `state::publish_abc` / `state::publish_nom` by the tests below.
const ABC_ASSIGNMENTS: &str = "abc.assignments";
const ABC_BRANCHES: &str = "abc.branches";
const ABC_CONDITIONS: &str = "abc.conditions";
const NOM_FUNCTIONS: &str = "nom.functions";
const NOM_CLOSURES: &str = "nom.closures";

#[cfg(test)]
mod tests {
    use super::*;
    use mehen_core::{MetricKey, MetricSet};

    fn span(start: u32) -> SourceSpan {
        SourceSpan::new(start, start + 4, 1, 1)
    }

    #[test]
    fn disabled_sink_records_nothing() {
        let mut e = MetricEvidence::new("t", false);
        e.decision(span(0), "if");
        e.cognitive(span(4), 2, "if");
        e.exit(span(8), "return");
        e.abc_assignment(span(12), "assign");
        e.function(span(16), "def");
        e.public_method(span(20), "method");
        assert!(e.finish().is_empty());
    }

    #[test]
    fn reason_codes_are_language_prefixed_and_detail_optional() {
        let mut e = MetricEvidence::new("c", true);
        e.decision(span(0), "if_statement");
        e.function(span(4), "");
        let entries = e.finish();
        assert_eq!(entries[0].reason.as_str(), "c.cyclomatic.if_statement");
        assert_eq!(entries[1].reason.as_str(), "c.nom.function");
    }

    #[test]
    fn zero_amount_events_are_skipped() {
        let mut e = MetricEvidence::new("t", true);
        e.cognitive(span(0), 0, "same_op_boolean");
        e.function_args(span(4), 0, "def");
        e.closure_args(span(8), 0, "lambda");
        assert!(e.finish().is_empty());
    }

    #[test]
    fn amounts_carry_the_applied_delta() {
        let mut e = MetricEvidence::new("t", true);
        e.cognitive(span(0), 3, "nested_if");
        e.function_args(span(4), 5, "def");
        let entries = e.finish();
        assert_eq!(entries[0].amount, 3.0);
        assert_eq!(entries[1].amount, 5.0);
    }

    #[test]
    fn contribution_metric_keys_match_published_key_names() {
        // Evidence must attach to keys that `apply_state_to` actually
        // publishes, so a report reader can join contributions to the
        // metric table. Build a State, publish it, and assert every
        // evidence key is present in the output.
        let mut state = crate::State::new();
        state.nom.record_function();
        state.npa.record_class_like();
        state.npm.record_class_like();
        crate::finalize_state(&mut state);
        let mut set = MetricSet::new();
        crate::apply_state_to(state, &mut set);

        let mut e = MetricEvidence::new("t", true);
        e.decision(span(0), "d");
        e.cognitive(span(1), 1, "c");
        e.exit(span(2), "e");
        e.abc_assignment(span(3), "a");
        e.abc_branch(span(4), "b");
        e.abc_condition(span(5), "c");
        e.function(span(6), "f");
        e.closure(span(7), "l");
        e.function_args(span(8), 1, "f");
        e.closure_args(span(9), 1, "l");
        e.public_attribute(span(10), "a");
        e.public_method(span(11), "m");

        for entry in e.finish() {
            assert!(
                set.get(&MetricKey::new(entry.metric.as_str())).is_some(),
                "evidence key `{}` is not a published metric key",
                entry.metric
            );
        }
    }
}
