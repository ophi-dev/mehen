// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! `mehen-powershell` — PowerShell language analyzer.
//!
//! Phase 3 implementation: walks tree-sitter-pwsh with PowerShell-specific
//! decision rules mirroring the pre-1.0 `Cyclomatic for PowershellCode`
//! (`src/metrics/cyclomatic.rs:250-306`).

#![forbid(unsafe_code)]

use mehen_core::{
    AnalysisBackend, AnalysisConfig, Language, LanguageAnalysis, LanguageAnalyzer, ParseDiagnostic,
    Result, SourceFile, SourceSpan, SpaceKind, byte_offset_clamped,
};
use mehen_tree_sitter::{
    CognitiveFact, LanguageRules, MetricEvidence, NodeFacts, ScopeOpen, TreeSitterParser,
    collect_recovered_errors, empty_space, text_of, walk,
};
use tree_sitter::Node;

pub struct PowerShellAnalyzer;

impl PowerShellAnalyzer {
    pub fn new() -> Self {
        Self
    }
}

impl Default for PowerShellAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

struct PowerShellRules;

impl LanguageRules for PowerShellRules {
    fn scope_for(&self, node: &Node<'_>, source: &[u8]) -> Option<ScopeOpen> {
        let kind = node.kind();
        // Mirrors the pre-1.0 `Checker for PowershellCode::is_func_space`
        // (`src/checker.rs`): `Program` (the unit, handled by the walker
        // separately), `FunctionStatement`, `ClassStatement`,
        // `ClassMethodDefinition`, and `ScriptBlockExpression` open a
        // space. The bare `script_block` *does not* open a space — it's
        // the body container for switch-clause arms etc., not a closure.
        let opened = match kind {
            "function_statement" | "function_definition" => ScopeOpen::Open {
                kind: SpaceKind::Function,
                name: node
                    .child_by_field_name("name")
                    .map(|n| text_of(&n, source).to_string()),
            },
            "class_method_definition" => ScopeOpen::Open {
                kind: SpaceKind::Function,
                name: node
                    .child_by_field_name("name")
                    .map(|n| text_of(&n, source).to_string()),
            },
            "script_block_expression" => ScopeOpen::Open {
                kind: SpaceKind::Closure,
                name: None,
            },
            "class_statement" => ScopeOpen::Open {
                kind: SpaceKind::Class,
                name: node
                    .child_by_field_name("name")
                    .map(|n| text_of(&n, source).to_string()),
            },
            _ => return None,
        };
        Some(opened)
    }

    fn classify(&self, node: &Node<'_>) -> NodeFacts {
        let kind = node.kind();
        // Per pre-1.0 src/metrics/cyclomatic.rs:250-306. PowerShell adds
        // -and / -or as short-circuit and v7's null-coalesce / ternary.
        let cyclomatic_decision = matches!(
            kind,
            "if_statement"
                | "elseif_clause"
                | "for_statement"
                | "foreach_statement"
                | "while_statement"
                | "do_statement"
                | "switch_clause"
                | "catch_clause"
                | "trap_statement"
                | "ternary_expression"
                | "ternary_argument_expression"
                | "null_coalesce_expression"
                | "null_coalesce_argument_expression"
                | "&&"
                | "||"
                | "-and"
                | "-or"
        );
        // NExit: `return`, `throw`, `exit` — but not `break` / `continue`
        // (loop-local flow, not function exit). tree-sitter-pwsh emits all
        // of those as `flow_control_statement` whose first child is the
        // specific keyword token, so we match on the leading child.
        let nexit = kind == "flow_control_statement"
            && matches!(
                node.child(0).map(|c| c.kind()),
                Some("return") | Some("throw") | Some("exit")
            );
        let halstead_operator = is_powershell_operator(kind);
        let halstead_operand = is_powershell_operand(kind);
        // ABC classification per pre-1.0 `Abc for PowershellCode`
        // (`src/metrics/abc.rs:562-632`).
        let abc_assignment = matches!(
            kind,
            "assignment_expression"
                | "pre_increment_expression"
                | "pre_decrement_expression"
                | "post_increment_expression"
                | "post_decrement_expression"
        );
        let abc_branch = matches!(kind, "command" | "invocation_expression");
        // Conditions: structural conditionals + comparison / ternary /
        // null-coalesce wrappers (these wrap a single operator each, so
        // matching them doesn't double-count) + the leaf logical
        // operator tokens. Intentionally NOT `logical_expression` /
        // `logical_argument_expression` / `pipeline_chain` — those
        // wrappers can hold multiple leaves, so matching them too
        // would double-count.
        let abc_condition = matches!(
            kind,
            "if_statement"
                | "elseif_clause"
                | "for_statement"
                | "foreach_statement"
                | "while_statement"
                | "do_statement"
                | "switch_clause"
                | "catch_clause"
                | "trap_statement"
                | "ternary_expression"
                | "ternary_argument_expression"
                | "null_coalesce_expression"
                | "null_coalesce_argument_expression"
                | "comparison_expression"
                | "comparison_argument_expression"
                | "&&"
                | "||"
                | "-and"
                | "-or"
                | "-xor"
        );
        NodeFacts {
            cyclomatic_decision,
            cognitive: powershell_cognitive_fact(node),
            halstead_operator,
            halstead_operand,
            nexit,
            abc_branch,
            abc_condition,
            abc_assignment,
            loc: powershell_loc_fact(node),
        }
    }

    fn count_args(&self, node: &Node<'_>, _source: &[u8]) -> u32 {
        powershell_count_args(node)
    }

    fn classify_attribute(
        &self,
        node: &Node<'_>,
        _source: &[u8],
    ) -> Option<mehen_tree_sitter::MemberClassification> {
        // PowerShell properties are `class_property_definition` direct
        // children of a `class_statement`. PowerShell has no
        // access-modifier equivalent to `private` / `protected`; the
        // `hidden` keyword only suppresses default Get-Member output —
        // members remain publicly accessible. Per `about_Hidden`:
        // "hidden members are still public".
        if node.kind() != "class_property_definition" {
            return None;
        }
        let in_class = node.parent().is_some_and(|p| p.kind() == "class_statement");
        if !in_class {
            return None;
        }
        Some(mehen_tree_sitter::MemberClassification {
            container: mehen_metrics::ContainerKind::Class,
            is_public: true,
        })
    }

    fn classify_method(
        &self,
        node: &Node<'_>,
        _source: &[u8],
    ) -> Option<mehen_tree_sitter::MemberClassification> {
        if node.kind() != "class_method_definition" {
            return None;
        }
        let in_class = node.parent().is_some_and(|p| p.kind() == "class_statement");
        if !in_class {
            return None;
        }
        Some(mehen_tree_sitter::MemberClassification {
            container: mehen_metrics::ContainerKind::Class,
            is_public: true,
        })
    }
}

/// Count the function/closure parameters declared by the
/// PowerShell space rooted at `node`. Mirrors the pre-1.0
/// `compute_powershell_args` (`src/metrics/nargs.rs:293-370`):
/// PowerShell parameter declarations appear in three shapes —
/// `function_statement` > `function_parameter_declaration` >
/// `parameter_list` > `script_parameter`; `script_block_expression` >
/// `param_block` > `parameter_list` > `script_parameter`; or
/// `class_method_definition` > `class_method_parameter_list` >
/// `class_method_parameter`. The walker recurses ONLY through the
/// thin structural wrappers between the entry node and the parameter
/// list — never into the body — so nested closures don't leak args
/// into their enclosing function.
fn powershell_count_args(node: &Node<'_>) -> u32 {
    let kind = node.kind();
    let is_method = kind == "class_method_definition";
    let mut count: u32 = 0;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "parameter_list" if !is_method => {
                let mut pl = child.walk();
                for p in child.children(&mut pl) {
                    if p.kind() == "script_parameter" {
                        count = count.saturating_add(1);
                    }
                }
            }
            "class_method_parameter_list" if is_method => {
                let mut pl = child.walk();
                for p in child.children(&mut pl) {
                    if p.kind() == "class_method_parameter" {
                        count = count.saturating_add(1);
                    }
                }
            }
            // Recurse only into the structural wrapper that directly
            // contains the parameter list — `function_parameter_declaration`
            // for functions, `param_block` for closures. The body
            // `script_block` / `script_block_body` / `statement_list` is
            // intentionally NOT descended (that's where nested closures
            // live).
            "function_parameter_declaration" | "param_block" => {
                count = count.saturating_add(powershell_count_args(&child));
            }
            _ => {}
        }
    }
    count
}

/// PowerShell Halstead operator classification per pre-1.0
/// `Getter::get_op_type for PowershellCode` (`src/getter.rs:485-548`).
/// Wrapper rule kinds (`assignment_operator`, `comparison_operator`,
/// `format_operator`, `file_redirection_operator`,
/// `merging_redirection_operator`) are intentionally NOT included —
/// the walker visits both the wrapper and its leaf token, and matching
/// only the leaves prevents double-counting.
fn is_powershell_operator(kind: &str) -> bool {
    matches!(
        kind,
        // Keywords and structural / control-flow markers.
        "function" | "filter" | "workflow" | "if" | "elseif" | "else"
        | "switch" | "for" | "foreach" | "in" | "while" | "do" | "until"
        | "break" | "continue" | "return" | "throw" | "exit"
        | "try" | "catch" | "finally" | "trap"
        | "param" | "using" | "namespace" | "module" | "assembly"
        | "static" | "this" | "base"
        | "begin" | "process" | "end" | "clean" | "dynamicparam"
        | "data" | "inlinescript" | "parallel" | "sequence"
        // Punctuation-like.
        | "(" | "{" | "[" | "," | ";" | "." | ".." | ":" | "::"
        | "@(" | "@{" | "$("
        // Assignment family.
        | "=" | "+=" | "-=" | "*=" | "/=" | "%=" | "??="
        // Arithmetic / bitwise / unary.
        | "+" | "-" | "*" | "/" | "%" | "\\" | "..."
        | "++" | "--" | "!"
        // Short-circuit / null-coalesce / ternary.
        | "&&" | "||" | "?" | "??"
        // Pipeline / invocation / redirection.
        | "|" | "&"
        // Word-form logical / comparison / typing operators.
        | "-and" | "-or" | "-xor" | "-not"
        | "-band" | "-bor" | "-bxor" | "-bnot"
        | "-as" | "-is" | "-isnot"
        | "-f" | "-join"
        | "-shl" | "-shr"
        | "-split" | "-isplit" | "-csplit"
        | "-replace" | "-ireplace" | "-creplace"
        | "-match" | "-imatch" | "-cmatch"
        | "-notmatch" | "-inotmatch" | "-cnotmatch"
        | "-like" | "-ilike" | "-clike"
        | "-notlike" | "-inotlike" | "-cnotlike"
        | "-contains" | "-icontains" | "-ccontains"
        | "-notcontains" | "-inotcontains" | "-cnotcontains"
        | "-in" | "-notin"
        | "-eq" | "-ieq" | "-ceq" | "-ne" | "-ine" | "-cne"
        | "-lt" | "-ilt" | "-clt" | "-le" | "-ile" | "-cle"
        | "-gt" | "-igt" | "-cgt" | "-ge" | "-ige" | "-cge"
        | "<" | ">"
        // File / merging redirection leaf tokens. Names mirror
        // tree-sitter-pwsh's anonymous tokens for `2>`, `2>>`,
        // `2>&1`, `*>`, `3>&2`, etc.
        | ">>" | "*>" | "*>>" | "*>&1" | "*>&2"
        | "2>" | "2>>" | "2>&1"
        | "3>" | "3>>" | "3>&1" | "3>&2"
        | "4>" | "4>>" | "4>&1" | "4>&2"
        | "5>" | "5>>" | "5>&1" | "5>&2"
        | "6>" | "6>>" | "6>&1" | "6>&2"
        | "1>&2"
    )
}

/// PowerShell Halstead operand classification per pre-1.0
/// `Getter::get_op_type for PowershellCode` (`src/getter.rs:485-593`).
fn is_powershell_operand(kind: &str) -> bool {
    matches!(
        kind,
        // Identifiers, type names, variables.
        "simple_name" | "type_identifier" | "variable" | "braced_variable"
        | "generic_token"
        // Numeric literals.
        | "decimal_integer_literal" | "hexadecimal_integer_literal" | "real_literal"
        // Verbatim (single-quoted) string content leaves.
        | "verbatim_string_characters" | "verbatim_here_string_characters"
        // Expandable (double-quoted) string wrappers — counted as one
        // operand each because the wrapper's byte range carries the
        // text directly (no content-leaf node).
        | "expandable_string_literal" | "expandable_here_string_literal"
        // Identifier leaves driving function declarations and command
        // invocations. The named wrappers `command_name_expr` /
        // `path_command_name` are intentionally NOT included to avoid
        // double-counting against their leaf token.
        | "function_name" | "command_name" | "path_command_name_token"
        | "command_parameter"
    )
}

/// PowerShell cognitive-complexity classification per pre-1.0
/// `Cognitive for PowershellCode` (`src/metrics/cognitive.rs:640-770`).
///
/// Returns one [`CognitiveFact`] describing how this node contributes
/// to the cognitive state machine; the walker drives the `(nesting,
/// depth, lambda)` context and the `BoolSequence` collapser based on
/// the variant.
fn powershell_cognitive_fact(node: &Node<'_>) -> CognitiveFact {
    use smol_str::SmolStr;
    let kind = node.kind();
    match kind {
        // Nesting-increasing constructs: `if` / loops / `switch` /
        // `catch` / ternary / null-coalesce. Each adds `nesting + 1`
        // and bumps the descendant nesting depth.
        "if_statement"
        | "for_statement"
        | "foreach_statement"
        | "while_statement"
        | "do_statement"
        | "switch_statement"
        | "catch_clause"
        | "ternary_expression"
        | "ternary_argument_expression"
        | "null_coalesce_expression"
        | "null_coalesce_argument_expression" => CognitiveFact::IncreaseNesting,
        // Same-level conditional clauses: +1 without bumping nesting,
        // and reset the boolean-sequence tracker.
        "elseif_clause" | "else_clause" | "finally_clause" | "trap_statement" => {
            CognitiveFact::NonNestingPlusOne
        }
        // Pipeline statements: statement-boundary reset + collect
        // `&&` / `||` from `pipeline_chain_tail` children for the
        // boolean-sequence collapser.
        "pipeline" => {
            let mut ops: Vec<SmolStr> = Vec::new();
            let mut cur = node.walk();
            for child in node.children(&mut cur) {
                if child.kind() != "pipeline_chain_tail" {
                    continue;
                }
                if let Some(op) = child.child(0) {
                    let op_kind = op.kind();
                    if matches!(op_kind, "&&" | "||") {
                        ops.push(SmolStr::new(op_kind));
                    }
                }
            }
            CognitiveFact::StatementBoundaryWithBooleans(ops)
        }
        // Assignment is also a statement boundary for the bool-sequence.
        "assignment_expression" => CognitiveFact::StatementBoundary,
        // Negation operators — track in the bool-sequence collapser
        // without bumping structural so a leading `!` / `-not` doesn't
        // mistake the next real boolean for a transition.
        "-not" | "!" | "-bnot" => CognitiveFact::NotOperator(SmolStr::new(kind)),
        // Logical wrappers carry one or more `-and` / `-or` / `-xor`
        // leaf tokens. Feed each leaf into the BoolSequence collapser.
        // The wrapper itself is not a statement boundary, so this is a
        // `BooleanContainer` (no reset).
        "logical_expression" | "logical_argument_expression" => {
            let mut ops: Vec<SmolStr> = Vec::new();
            let mut cur = node.walk();
            for child in node.children(&mut cur) {
                let k = child.kind();
                if matches!(k, "-and" | "-or" | "-xor") {
                    ops.push(SmolStr::new(k));
                }
            }
            CognitiveFact::BooleanContainer(ops)
        }
        // Function-like spaces reset structural nesting and bump the
        // `depth` so children of nested functions count their own
        // nesting from zero.
        "function_statement" | "class_method_definition" => CognitiveFact::FunctionEntry,
        // Closures (script-block expressions) bump `lambda` so their
        // descendants pay the lambda penalty.
        "script_block_expression" => CognitiveFact::LambdaEntry,
        _ => CognitiveFact::None,
    }
}

/// PowerShell LOC classification per pre-1.0
/// `Loc for PowershellCode` (`src/metrics/loc.rs:909-961`).
fn powershell_loc_fact(node: &Node<'_>) -> mehen_tree_sitter::LocFact {
    use mehen_tree_sitter::LocFact;
    match node.kind() {
        // Containers — must NOT contribute to PLOC.
        "program" | "script_block" | "script_block_body" | "statement_list" | "statement_block"
        | "named_block_list" | "named_block" | "param_block" | "elseif_clauses"
        | "catch_clauses" | "switch_body" | "switch_clauses" => LocFact::Container,
        // Comments cover both `#` line comments and `<# ... #>` block
        // comments — they share the `comment` named rule in tree-sitter-pwsh.
        "comment" => LocFact::Comment,
        // LLOC: each statement-shaped node bumps LLOC once. The
        // tree-sitter-pwsh v0.37+ grammar emits one `pipeline` per
        // statement (the assignment RHS is a dedicated `assignment_value`
        // rather than a nested `pipeline`), so counting every visible
        // `pipeline` once is safe.
        "pipeline"
        | "if_statement"
        | "for_statement"
        | "foreach_statement"
        | "while_statement"
        | "do_statement"
        | "switch_statement"
        | "try_statement"
        | "trap_statement"
        | "function_statement"
        | "class_statement"
        | "enum_statement"
        | "data_statement"
        | "flow_control_statement"
        | "class_method_definition"
        | "class_property_definition" => LocFact::Lloc,
        _ => LocFact::Code,
    }
}

impl LanguageAnalyzer for PowerShellAnalyzer {
    fn language(&self) -> Language {
        Language::PowerShell
    }

    fn backend(&self) -> AnalysisBackend {
        AnalysisBackend::TreeSitter
    }

    fn analyze(&self, source: &SourceFile, config: &AnalysisConfig) -> Result<LanguageAnalysis> {
        let parser = match TreeSitterParser::new(
            tree_sitter_pwsh::LANGUAGE.into(),
            source.text.clone().into_bytes(),
        ) {
            Ok(p) => p,
            Err(e) => {
                let span = SourceSpan {
                    start_byte: 0,
                    end_byte: byte_offset_clamped(source.text.len()),
                    start_line: 1,
                    end_line: source.line_index.line_count(),
                };
                return Ok(LanguageAnalysis {
                    language: Language::PowerShell,
                    backend: AnalysisBackend::TreeSitter,
                    diagnostics: vec![ParseDiagnostic::fatal(
                        "powershell.parse_error",
                        format!("tree-sitter-pwsh failed: {e}"),
                    )],
                    root: empty_space(span),
                    contributions: Vec::new(),
                });
            }
        };

        let mut evidence = MetricEvidence::new("powershell", config.emit_contributions);
        let result = walk(
            parser.root(),
            parser.source(),
            &source.line_index,
            &PowerShellRules,
            &mut evidence,
        );
        // Tree-sitter recovers from syntax errors by inserting ERROR /
        // missing nodes; surface them as `error` diagnostics so the
        // metric output can't masquerade as clean (plan §9.3).
        let diagnostics = collect_recovered_errors(parser.root(), "powershell.syntax_error", 16);
        // Per-space McCabe base rows (+1 per space, unit included) so
        // cyclomatic evidence sums to `cyclomatic.sum` — decisions alone
        // cannot explain the rolled-up value (an empty function moves it).
        evidence.record_cyclomatic_bases(&result.root);
        Ok(LanguageAnalysis {
            language: Language::PowerShell,
            backend: AnalysisBackend::TreeSitter,
            diagnostics,
            root: result.root,
            contributions: evidence.finish(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mehen_core::{AnalysisConfig, Language, SourceFile};

    #[test]
    fn analyzes_simple_script() {
        let a = PowerShellAnalyzer::new()
            .analyze(
                &SourceFile::new(
                    "a.ps1".into(),
                    Language::PowerShell,
                    "function Foo { 1 }".to_string(),
                ),
                &AnalysisConfig::default(),
            )
            .unwrap();
        assert_eq!(a.root.kind, SpaceKind::Unit);
    }
}
