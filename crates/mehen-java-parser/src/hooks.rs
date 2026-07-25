// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Hand-written Rust port of the upstream `JavaParserBase` helper class.
//!
//! The vendored grammar declares `superClass = JavaParserBase` and calls two
//! of its predicates (`this.IsNotIdentifierAssign()`,
//! `this.DoLastRecordComponent()`). `grammar/patterns.toml` lowers both to
//! typed hooks, and this module supplies the exact semantics — a line-for-line
//! port of `java/java/Java/JavaParserBase.java` from `antlr/grammars-v4` (the
//! commit pinned in `grammar/PROVENANCE.md`).
//!
//! Construct the parser with [`JavaParserBase`] installed:
//!
//! ```ignore
//! let mut parser = JavaParser::with_typed_hooks(tokens, JavaParserBase);
//! ```
//!
//! The generated modules are produced with `--sem-unknown error`, so a parser
//! built *without* these hooks (e.g. via `JavaParser::new`) fails loud with
//! `AntlrError::Unsupported` the moment an input reaches either predicate —
//! it never silently mis-parses.

use antlr4_runtime::{ParserSemCtx, TokenSource};

use crate::java_lexer::{
    ASSIGN, ELLIPSIS, EXPORTS, IDENTIFIER, MODULE, OPEN, OPENS, PERMITS, PROVIDES, RECORD,
    REQUIRES, SEALED, TO, TRANSITIVE, USES, VAR, WHEN, WITH, YIELD,
};
use crate::java_parser::{JavaParserHooks, RULE_RECORD_COMPONENT, RULE_RECORD_COMPONENT_LIST};

/// Rust port of the upstream `JavaParserBase` (see module docs). Stateless:
/// both predicates read only the token stream and the in-flight rule context.
#[derive(Clone, Copy, Debug, Default)]
pub struct JavaParserBase;

/// Token types that can begin the `identifier` parser rule — `IDENTIFIER`
/// plus every contextual keyword the grammar folds back into identifiers.
/// Mirrors the `switch` arms in the upstream `IsNotIdentifierAssign`.
const IDENTIFIER_LIKE: [i32; 17] = [
    IDENTIFIER, MODULE, OPEN, REQUIRES, EXPORTS, OPENS, TO, USES, PROVIDES, WHEN, WITH, TRANSITIVE,
    YIELD, SEALED, PERMITS, RECORD, VAR,
];

impl JavaParserHooks for JavaParserBase {
    /// `annotationFieldValue: { this.IsNotIdentifierAssign() }? annotationValue
    ///                       | identifier '=' annotationValue`
    ///
    /// True unless the lookahead is `<identifier-like> =`, steering named
    /// annotation arguments (`@Foo(bar = 1)`) to the explicit
    /// `identifier '=' annotationValue` alternative instead of parsing
    /// `bar = 1` as an assignment *expression*.
    fn is_not_identifier_assign<L>(&mut self, ctx: &mut ParserSemCtx<'_, L>) -> bool
    where
        L: TokenSource,
    {
        if !IDENTIFIER_LIKE.contains(&ctx.la(1)) {
            return true;
        }
        ctx.la(2) != ASSIGN
    }

    /// `recordComponentList: recordComponent (',' recordComponent)*
    ///                       { this.DoLastRecordComponent() }?`
    ///
    /// False when a varargs component is followed by another component,
    /// rejecting `record R(int... xs, int y)` at parse time (only the last
    /// record component may be `...`), as `javac` does.
    fn do_last_record_component<L>(&mut self, ctx: &mut ParserSemCtx<'_, L>) -> bool
    where
        L: TokenSource,
    {
        // Upstream guards `getContext() instanceof RecordComponentListContext`
        // and accepts otherwise; an absent context (speculative prediction
        // outside the rule) is the same "unexpected state" and also accepts.
        let Some(context) = ctx.context() else {
            return true;
        };
        if context.rule_index() != RULE_RECORD_COMPONENT_LIST {
            return true;
        }
        let storage = ctx.parse_tree_storage();
        let tokens = ctx.token_store();
        let mut components = context
            .child_rules(storage, tokens, RULE_RECORD_COMPONENT)
            .peekable();
        while let Some(component) = components.next() {
            // `rc.ELLIPSIS() != null` upstream: the `...` terminal is a direct
            // child of `recordComponent` (`annotation* typeType (annotation*
            // ELLIPSIS)? identifier`), so only non-last components matter.
            if components.peek().is_some() && component.has_token(ELLIPSIS) {
                return false;
            }
        }
        true
    }
}
