// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Recovered-error collection for ANTLR parse trees.
//!
//! Like tree-sitter, the ANTLR runtime recovers from syntax errors rather
//! than aborting: bad input surfaces as `ParseTree::Error` leaves embedded
//! in an otherwise-complete tree. Per mehen's diagnostic contract (rewrite
//! plan §9.3) these must be reported as `error`-severity diagnostics so
//! that `mehen metrics` exits 1 and `mehen diff` records the file under
//! `analysis_errors` — metric output from a partially-recovered tree must
//! never masquerade as a clean parse.
//!
//! Hard parser failures (the entry-rule call itself returning `Err`) are a
//! separate, `fatal` path handled by the analyzer crate; this module only
//! covers recovered `Error` nodes within a returned tree.

use antlr4_runtime::token::Token;
use antlr4_runtime::{ParseTree, ParserRuleContext};
use mehen_core::ParseDiagnostic;

/// Walk `tree` and emit one `error`-severity [`ParseDiagnostic`] per
/// recovered `ParseTree::Error` leaf, capped at `max_diagnostics` to bound
/// noise on heavily corrupted input.
///
/// `code` is the language-namespaced diagnostic code, e.g.
/// `"kotlin.syntax_error"`. Returns an empty `Vec` for a clean parse.
pub fn collect_errors(
    tree: &ParseTree,
    code: &str,
    max_diagnostics: usize,
) -> Vec<ParseDiagnostic> {
    let mut out = Vec::new();
    collect_into(tree, code, max_diagnostics, &mut out);
    out
}

fn collect_into(tree: &ParseTree, code: &str, max: usize, out: &mut Vec<ParseDiagnostic>) {
    if out.len() >= max {
        return;
    }
    match tree {
        ParseTree::Error(err) => {
            let line = err.symbol().line();
            out.push(ParseDiagnostic::error(
                code.to_string(),
                format!("ANTLR error node at line {line}"),
            ));
        }
        ParseTree::Rule(rule) => {
            for child in rule.context().children() {
                collect_into(child, code, max, out);
                if out.len() >= max {
                    return;
                }
            }
        }
        ParseTree::Terminal(_) => {}
    }
}

/// Returns the first child rule context with `rule_index` directly under
/// `ctx`, if any. A small convenience used by language walkers that need a
/// specific child rule (e.g. a class declaration's `class_body`).
pub fn child_rule(ctx: &ParserRuleContext, rule_index: usize) -> Option<&ParserRuleContext> {
    ctx.children().iter().find_map(|child| match child {
        ParseTree::Rule(rule) if rule.context().rule_index() == rule_index => Some(rule.context()),
        _ => None,
    })
}

/// Returns true if any direct child terminal of `ctx` has the given token
/// type. Languages use this for keyword-presence checks (e.g. does this
/// `classDeclaration` carry an `INTERFACE` token?).
pub fn has_child_token(ctx: &ParserRuleContext, token_type: i32) -> bool {
    ctx.children().iter().any(|child| match child {
        ParseTree::Terminal(t) => t.symbol().token_type() == token_type,
        _ => false,
    })
}
