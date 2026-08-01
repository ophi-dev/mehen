#!/usr/bin/env -S uv run --script
# /// script
# # A developer build step, not a shipped artifact, so the floor is simply a
# # currently-supported interpreter — 3.9 reached end of life in October 2025.
# # 3.12 matches the version CI already installs (.github/workflows/release.yml).
# # Unrelated to `pyproject.toml`'s `requires-python`, which governs who can
# # install the mehen CLI. Raising this floor costs nothing because `uv` fetches a
# # matching interpreter when the machine lacks one.
# requires-python = ">=3.12"
# dependencies = []
# ///
"""Derive a generatable ANTLR grammar pair from Roslyn's published C# grammar.

`dotnet/roslyn` publishes `CSharp.Generated.g4`, machine-generated from
`Syntax.xml` — the same model that generates the compiler's own syntax nodes.
It therefore tracks C# *as implemented*, which no community grammar does. But
it is a **reference** grammar, not a working parser. Three classes of problem
have to be repaired before it parses real C#:

1.  **ANTLR rejects it outright.** Two `/* epsilon */` rules model Roslyn's
    *omitted* syntax nodes (the blank slots in `Foo<,>` / `new int[,]`), and four
    `/* see lexical specification */` rules are token stubs. An empty rule inside
    a closure is `error(153)`, which propagates to 23 errors across
    `compilation_unit`, every type declaration, and the XML doc-comment rules.
    `incomplete_member` and the XML trivia wrappers are all-optional, making
    `member_declaration*` and friends nullable.

2.  **There is no lexer.** The grammar is parser-only: terminals are inline
    literals plus character-level rules (`decimal_digit : '0' | '1' | …`,
    `identifier_token : '@'? identifier_start_character …`). Those must move into
    a real lexer, or single-character tokens shadow multi-character ones (`'C'`
    beats `IDENTIFIER`, `'1'` beats a decimal literal). Two tokens are valid only
    inside a lexer *mode*: interpolated-string text and XML doc-comment text.

3.  **It generates cleanly and still mis-parses.** The larger group, and the one
    that cost the most to find: contextual keywords harvested into reserved
    tokens (`var`, `record`, `_`, …), `>>` eating generic closers, accessor
    bodies with no bare `;`, missing `out`/`in`/`params` parameter modifiers,
    absent binary literals, and `<Choice>` in `Syntax.xml` rendering stricter
    than Roslyn's own hand-written parser. Each is catalogued with its measured
    effect in `PROVENANCE.md`; none is detectable from the grammar text alone —
    they surface only by parsing real C# and comparing against the language.

A separate performance repair also lives here: Roslyn writes every type body as
`'{'? member_declaration* '}'?`, which makes prediction quadratic in members per
type (128 members: 22.5 s → 0.37 s once the braces are a balanced pair).

This script is a **step of parser generation**, not a one-off: `cargo xtask antlr
generate csharp` runs it before `antlr4-rust-gen`. Only the vendored
`CSharp.Generated.g4` is checked in; the `CSharpLexer.g4` / `CSharpParser.g4` pair
and `patterns.toml` it emits are gitignored build artifacts, so the upstream
grammar stays the single source of truth exactly as the raw `.g4` does for Kotlin
and Java. See `PROVENANCE.md` for the pinned revision and the full catalogue of
what the transform repairs and why.

Interpolation additionally needs three lexer modes and brace-depth state, because
the `}` closing a hole is lexically identical to one closing a nested block. All of
it lives in the *grammar* — `@lexer::members` plus predicate-gated rules over the
same character — so the parser crate needs **no hooks at all**. That is deliberate:
mehen is the demonstrative consumer of antlr-rust-runtime, so pushing its
`--sem-patterns` lowering as far as it goes is the point, not a cost.

Reachability is delegated, not reimplemented: the generator's `G4S078` analysis
walks the real grammar AST, so it distinguishes a rule reference from a word
inside an action, a label, or an argument list. Hence `antlr4-rust-gen` is a hard
requirement (`--generator`, `$MEHEN_ANTLR_RUST_GEN`, or `PATH`) — see
`unreachable_rules`.

Usage:
    uv run prepare-grammar.py CSharp.Generated.g4 --out-dir .

Running it by hand is only for iterating on the transform.
"""

import argparse
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

# ---------------------------------------------------------------------------
# Rules whose bodies belong in a lexer, mapped to the token they become.
#
# Each entry replaces the parser rule's body with a single token reference and
# contributes one lexer rule. The lexer bodies are written to match the C#
# specification's lexical grammar (ECMA-334 §6.4) for the construct the Roslyn
# rule describes.
# ---------------------------------------------------------------------------
# Roslyn parser rule -> the lexer token that replaces its body. The token's
# ANTLR definition lives in `lexer-tokens.g4.in` (hand-written grammar source,
# spliced in verbatim below) so its escaping is readable as grammar rather than
# as nested Python string escapes.
LEXER_TOKEN_RULES: dict[str, str] = {
    "identifier_token": "IDENTIFIER",
    "decimal_integer_literal_token": "DEC_INT_LIT",
    "hexadecimal_integer_literal_token": "HEX_INT_LIT",
    "real_literal_token": "REAL_LIT",
    "character_literal_token": "CHAR_LIT",
    "regular_string_literal_token": "STRING_LIT",
    "verbatim_string_literal_token": "VERBATIM_STRING_LIT",
    "multi_line_raw_string_literal_token": "ML_RAW_STRING_LIT",
    "single_line_raw_string_literal_token": "SL_RAW_STRING_LIT",
    # Mode-scoped: only valid inside an interpolated string / XML doc comment.
    "interpolated_string_text_token": "INTERPOLATED_TEXT",
    "xml_text_literal_token": "XML_TEXT_LIT",
}

# Hand-written ANTLR source spliced into the emitted lexer (see the file's own
# header for why it is kept out of this script).
LEXER_RULES_FILE = "lexer-tokens.g4.in"

# The `@lexer::members` block holding interpolated-string state. Separate from the
# rules file because ANTLR requires named actions in the grammar *header*, before
# any rule, while the rules must follow the harvested literal tokens.
LEXER_MEMBERS_FILE = "lexer-members.g4.in"

# The subset of LEXER_TOKEN_RULES whose tokens are produced from a lexer mode
# rather than the default mode, so they need a `tokens {}` declaration.
MODE_SCOPED_RULES = ("interpolated_string_text_token", "xml_text_literal_token")

# Rules that are all-optional and so make their closure-using callers nullable.
# Each is tightened to require at least one element; the XML wrappers are `x*` over a
# nullable element.
#
# `incomplete_member` used to be listed here too — it is all-optional, so it matched
# the empty string. It is now removed from `member_declaration` outright (see
# INCOMPLETE_MEMBER_ALT) and pruned as unreachable, so there is nothing left to
# tighten.
NULLABILITY_FIXES = [
    (
        "xml_text\n  : xml_text_literal_token*\n  ;",
        "xml_text\n  : xml_text_literal_token+\n  ;",
    ),
    (
        "documentation_comment_trivia\n  : xml_node*\n  ;",
        "documentation_comment_trivia\n  : xml_node+\n  ;",
    ),
    (
        "skipped_tokens_trivia\n  : syntax_token*\n  ;",
        "skipped_tokens_trivia\n  : syntax_token+\n  ;",
    ),
]

# Roslyn's grammar generator reads only `<Kind>` children of a `<Field>`, not
# `<ContextualKind>`. `RecordDeclarationSyntax.Keyword` is declared as
#
#     <Field Name="Keyword" Type="SyntaxToken" Override="true">
#       <ContextualKind Name="RecordKeyword"/>
#     </Field>
#
# and it is the ONLY `<ContextualKind>` in all of `Syntax.xml` (versus 1018
# plain `<Kind>`), so it is the single field that hits that blind spot. The
# published grammar therefore contains no `'record'` literal at all and spells
# the keyword as the catch-all `syntax_token`, which accepts every identifier,
# keyword, literal, operator, and punctuation token.
#
# The cost is severe: `class` becomes viable as both `class_declaration` and
# `record_declaration`, so full-context prediction carries the impossible
# record path across every member boundary — ~quadratic in members per type
# (24 members took 13.5 s; one real 953-line file took 272 s).
#
# `record` is a *contextual* keyword — legal as an ordinary name (`int record =
# 1;`) — so it must NOT become a reserved token. (Reserving it silently
# mis-parses `record R(int X);` as two enum members plus a parenthesized
# expression, with zero reported errors.) Instead the declaration position is
# restricted to an identifier whose text is `record`, via a predicate that the
# pattern DSL lowers to a pure SemIR comparison — no hooks required.
RECORD_KEYWORD_TARGET = (
    "record_declaration\n  : attribute_list* modifier* syntax_token"
)
RECORD_KEYWORD_REPLACEMENT = (
    "record_declaration\n  : attribute_list* modifier* record_keyword"
)
RECORD_KEYWORD_RULE = """

// Contextual keyword. Roslyn's grammar never spells `record` as a literal at all —
// it carries the information in <ContextualKind Name="RecordKeyword"/>, which its
// grammar generator drops — so the prep mints a dedicated `KW_RECORD` token
// (RECORD_TOKEN_RULE) and `identifier_token` is widened with it, exactly as it is
// with every other contextual keyword. `record` therefore remains a legal name
// while the declaration position predicts on a token of its own.
//
// A dedicated token rather than `{IsRecordKeyword()}? IDENTIFIER`, which was the
// first approach and does work in isolation: a predicate cannot prune a path ANTLR
// has already *committed* to, and this rule sits at a position where several
// member forms overlap. With the predicate form, `record_declaration` had to stay
// after `base_method_declaration` among `member_declaration`'s alternatives — so
// `record R(int X);` matched `method_declaration` with `record` as the return type
// and parsed as a phantom method. Hoisting it instead put the record path on the
// committed path for an ordinary property (`T P { get => 1; set { } }` predicts
// `record_keyword` = `T`), and the predicate could not reject that either: it
// surfaced as a hard error on 29 corpus files. A real token removes the ambiguity
// at its source, so alternative order stops mattering.
record_keyword
  : KW_RECORD
  ;
"""

# The lexer rule for the minted token, appended to the harvested keyword block so it
# precedes `IDENTIFIER` (ANTLR breaks an equal-length match by rule order, and the
# keyword tokens are emitted before the hand-written rules).
RECORD_TOKEN_RULE = "KW_RECORD : 'record' ;"

# `record` must stay usable as an ordinary name, so the minted token joins the
# contextual set that widens `identifier_token`. Kept separate from the harvested
# literals because it is not one — nothing in Roslyn's grammar spells it.
RECORD_TOKEN_NAME = "KW_RECORD"

# Interpolated strings need lexer modes, so the tokens that delimit them cannot
# be plain harvested literals. These literals are therefore NOT harvested; the
# parser is rewritten to reference the named, mode-switching tokens that
# `lexer-tokens.g4.in` defines instead.
#
# `'{'`, `'}'` and `':'` are NOT harvested as plain literals: each has a
# hole-sensitive meaning, so `lexer-tokens.g4.in` defines predicate-gated rules
# for them ahead of the plain fallbacks (see HOLE_SENSITIVE_LITERALS). All the
# brace-depth bookkeeping lives in the grammar's own lexer actions, so the parser
# crate needs no hooks at all.
INTERP_TOKEN_LITERALS = {
    '$"': "INTERP_START",
    '$@"': "INTERP_VERBATIM_START",
}

# The two Roslyn rules for an interpolated *raw* string's opening fence. Roslyn
# spells each as three parser tokens (`DOLLAR+ TRIPLE_DQUOTE DQUOTE*`), but the
# text between holes needs its own lexer mode — in the default mode the `a` of
# `$"""a{x}"""` lexes as an IDENTIFIER — so both are retargeted at the single
# mode-pushing INTERP_RAW_START token. The single- and multi-line spellings are
# character-identical (they differ only in whether the content holds a newline),
# so one token serves both.
INTERP_RAW_START_RULES = (
    "interpolated_multi_line_raw_string_start_token",
    "interpolated_single_line_raw_string_start_token",
)
INTERP_RAW_START_TOKEN = "INTERP_RAW_START"

# Meaningful names for every operator and punctuation literal, replacing the
# index-based `OP_nnn` fallback.
#
# The fallback name is derived from a literal's position in a sorted list, so
# adding or removing *any* literal renumbers unrelated tokens. That is harmless
# inside the generated grammar, where names are only referenced by other
# generated text, but not for anything hand-written that has to name a token:
# `lexer-tokens.g4.in` uses `type(LBRACE)` commands, and `mehen-csharp`'s walker
# classifies metrics by token (`&&` for a cognitive boolean run, `++` for an ABC
# assignment, `?` for a conditional). Index names would silently rebind those on
# the next upstream grammar update — a metric regression with no compile error.
#
# Keyword literals need no entry: they are named from their own text
# (`KW_CLASS`), which is already stable.
# Literals whose meaning depends on whether the lexer is inside an interpolation
# hole, so `lexer-tokens.g4.in` defines them itself (gated rules first, then the
# plain fallback) rather than having them harvested into the literals block above
# the hand-written rules. See HOLE_SENSITIVE_LITERALS' use in step 6.
HOLE_SENSITIVE_LITERALS = frozenset({"{", "}", ":"})

STABLE_TOKEN_NAMES = {
    # Punctuation and delimiters.
    "{": "LBRACE",
    "}": "RBRACE",
    "(": "LPAREN",
    ")": "RPAREN",
    "[": "LBRACKET",
    "]": "RBRACKET",
    '"': "DQUOTE",
    ":": "COLON",
    "::": "COLON_COLON",
    ";": "SEMICOLON",
    ",": "COMMA",
    ".": "DOT",
    "..": "DOT_DOT",
    "#": "HASH",
    "$": "DOLLAR",
    "'''": "TRIPLE_QUOTE",
    '"""': "TRIPLE_DQUOTE",
    "\\'": "ESCAPED_QUOTE",
    "\\\\": "ESCAPED_BACKSLASH",
    # Arithmetic and bitwise.
    "+": "PLUS",
    "-": "MINUS",
    "*": "STAR",
    "/": "SLASH",
    "%": "PERCENT",
    "&": "AMP",
    "|": "PIPE",
    "^": "CARET",
    "~": "TILDE",
    "<<": "LT_LT",
    # Comparison and logic.
    "!": "BANG",
    "<": "LT",
    ">": "GT",
    "<=": "LE",
    ">=": "GE",
    "==": "EQ_EQ",
    "!=": "NE",
    "&&": "AMP_AMP",
    "||": "PIPE_PIPE",
    # Assignment and increment.
    "=": "EQ",
    "+=": "PLUS_EQ",
    "-=": "MINUS_EQ",
    "*=": "STAR_EQ",
    "/=": "SLASH_EQ",
    "%=": "PERCENT_EQ",
    "&=": "AMP_EQ",
    "|=": "PIPE_EQ",
    "^=": "CARET_EQ",
    "<<=": "LT_LT_EQ",
    "++": "PLUS_PLUS",
    "--": "MINUS_MINUS",
    # Null handling, lambda, and misc.
    "?": "QUESTION",
    "??": "QUESTION_QUESTION",
    "??=": "QUESTION_QUESTION_EQ",
    "=>": "ARROW",
    "->": "MINUS_GT",
    # XML doc-comment fragments Roslyn's grammar mentions.
    "</": "LT_SLASH",
    "/>": "SLASH_GT",
}

# Two more generator blind spots, in the same family as the `record`
# <ContextualKind> loss — information Roslyn's syntax model keeps in untyped or
# prose form that its grammar generator cannot see:
#
# 1. Accessor bodies. `AccessorDeclarationSyntax`'s Body / ExpressionBody /
#    SemicolonToken are all optional per the model's own PropertyComments
#    ("null if there are no braces", "the optional semicolon token"), but the
#    emitted rule requires `(block | (arrow_expression_clause ';'))`. A bare
#    `;` body — i.e. every auto-property `{ get; set; }`, C# 3-era syntax —
#    cannot parse. This led 128 of the 210 corpus error files.
#
# 2. Parameter modifiers. `ParameterSyntax.Modifiers` is an untyped
#    `SyntaxList<SyntaxToken>` with no <Kind> children, so the generator emits
#    the fixed declaration-modifier list, which lacks `out`, `in`, `params`,
#    and `this`. (`ref` parses only because it is also a declaration
#    modifier.) `void M(out D d)` fails while `void M(ref D d)` passes.
#    Added at the `parameter` rule, not the global `modifier` rule, so the
#    parameter-only keywords cannot leak into type/member declarations.
#
# Applied on the pristine literal forms, before literal harvesting.
# `declaration_expression : type variable_designation` is listed eleven
# alternatives *before* `invocation_expression : expression argument_list`, and
# both match `F(x)` — `F` as a type with `(x)` a parenthesized designation, or
# `F` as an expression with `(x)` an argument list. ANTLR takes the first viable
# alternative, so **every method call in every position** parsed as a declaration
# expression, and with zero reported errors.
#
# Roslyn's own parser resolves this semantically (it knows whether `F` names a
# type); a syntax-only grammar cannot, so alternative order has to carry it.
# Moving the declaration form last makes the common case right while leaving it
# to win where nothing else fits — `F(out int x)` still parses as a declaration,
# because `out int x` is not argument-list shaped.
#
# Same family as the `record` <ContextualKind> loss: information the compiler
# holds outside the grammar, which the published grammar therefore drops.
DECLARATION_EXPRESSION_ALT = "  | declaration_expression\n"

# `incomplete_member : attribute_list* modifier* type` is Roslyn's error-*recovery*
# node: it exists so the compiler can build a syntax tree for source that is being
# typed, where `public int` is a member the author has not finished. Roslyn'"'"'s parser
# emits a diagnostic alongside it; the published grammar carries only the node.
#
# So a syntax-only parser accepts `class C { int }` as a complete, error-free
# compilation unit. That directly contradicts mehen'"'"'s diagnostic contract, where a
# clean parse is what tells `mehen metrics` to exit 0 and `mehen diff` to trust the
# numbers — broken source has to be *visible*.
#
# Dropping the alternative makes the same input a syntax error, which is the honest
# answer. Nothing legal is lost: every real member form has its own rule, and this one
# matches only a type with no declarator after it.
INCOMPLETE_MEMBER_ALT = "  | incomplete_member\n"

# The same ordering hazard one level up, in `member_declaration`. Its alternatives
# are alphabetical, so `base_method_declaration` precedes `base_type_declaration` —
# and `record` is a contextual keyword, hence a legal `type`. So
#
#     record R(int X);
#
# the single most common record spelling, matches
# `method_declaration : … type … identifier_token … parameter_list … ';'` with
# `record` as the RETURN TYPE and `R` as the method name. ANTLR takes the first
# viable alternative, so every positional record parsed as a method — reported as a
# function space rather than a class, with no NPA/NPM/WMC container and no
# diagnostic. (`record class R { }` parsed correctly, which is why this survived: the
# explicit-kind form cannot match `method_declaration`.)
#
# This one needs BOTH halves, and each is useless alone:
#
# 1. A dedicated `KW_RECORD` token (RECORD_KEYWORD_RULE), so the record path is
#    selected by a token rather than by a predicate over `IDENTIFIER`. Reordering
#    alone does not work: with the predicate form, hoisting `record_declaration`
#    ahead of `base_method_declaration` put the record path on the *committed* path
#    for an ordinary property (`T P { get => 1; set { } }` predicts `record_keyword`
#    = `T`), and a predicate cannot prune a committed path — 29 corpus files failed
#    with hard errors.
# 2. Hoisting `record_declaration` ahead of `base_method_declaration`. The token
#    alone does not work either, because `record` must stay a legal identifier and
#    is therefore widened back into `identifier_token` — so it is still a viable
#    `type`, and `method_declaration` still matches first.
#
# With the real token in place the hoist is safe: `T P { … }` no longer predicts the
# record path at all, because `T` is not `KW_RECORD`.
RECORD_DECL_ALT = "  | record_declaration\n"
MEMBER_METHOD_ALT = "  | base_method_declaration\n"

# The pattern-combinator keywords (C# 9 `and` / `or` / `not`). Contextual, so
# widening `identifier_token` makes each a legal name — which is correct in general
# but wrong in one position: `single_variable_designation : identifier_token` sits
# inside `declaration_pattern : type variable_designation`, so `o is int and > 5`
# binds `and` as a *variable* named `and` declared of type `int`. The `> 5` is then
# orphaned and the combinator vanishes from the tree — the same silent-misparse shape
# as the `declaration_expression` ordering bug, and with zero reported errors.
#
# `binary_pattern` is listed FIRST among `pattern`'s alternatives, so ANTLR does try
# it before the declaration form. It loses anyway: the combinator only survives if
# `and` is not consumed as the designation, and by the time the ATN reaches that
# choice the designation alternative is already viable.
#
# Excluding the three from *this one rule* is the narrow fix. They stay legal
# identifiers everywhere else (a field or parameter named `and` still parses), and a
# variable genuinely named `and` in a declaration pattern — `o is int and` — is not
# valid C# anyway, since the compiler reads that as a combinator too.
COMBINATOR_KEYWORDS = ("and", "or", "not")

# `single_variable_designation` in its post-harvest tokenized form, and the
# replacement that keeps every contextual keyword EXCEPT the combinators.
DESIGNATION_RULE = "single_variable_designation\n  : identifier_token\n  ;"

# `Syntax.xml` wraps a member's body in a <Choice> of Body / ExpressionBody /
# SemicolonToken, which the generator renders as `(block | (arrow_expression_clause
# ';'))` — *requiring* one of the two. But <Choice> drives the SyntaxFactory
# overload set and doc comments, not the hand-written parser: an abstract,
# `extern`, `partial`, or interface member legitimately has **no** body, just a
# semicolon.
#
# Without the bodiless alternative those members cannot match their own rule and
# fall through to `global_statement` (Roslyn's C# 9 top-level-statement node),
# which happily accepts `void Scale(double f);` as an expression statement — so an
# interface method silently became a call expression, with zero reported errors.
#
# Same shape as the accessor and property-initializer gaps below: information the
# real parser holds outside the syntax model.
BODY_REQUIRING_RULES = (
    "method_declaration",
    "operator_declaration",
    "conversion_operator_declaration",
    "constructor_declaration",
    "destructor_declaration",
)

REQUIRED_BODY = "(block | (arrow_expression_clause ';'))"
OPTIONAL_BODY = "(block | (arrow_expression_clause ';') | ';')"

GENERATOR_GAP_FIXES = [
    # (0) `x => …` must take a bare identifier, not a whole `parameter`.
    #
    # `SimpleLambdaExpressionSyntax.Parameter` really is a `ParameterSyntax` in
    # Roslyn's model, so the grammar is faithful — but every element of `parameter`
    # is optional, including `type?`, which can match a parenthesized tuple type.
    # So `(a, b) => a + b` parses as the *simple* form with `(a, b)` as one
    # parameter's type, instead of the parenthesized form with two parameters.
    # Roslyn's parser distinguishes them lexically (a leading `(` picks the
    # parenthesized form); the grammar cannot, so the simple form is narrowed to
    # what "simple" means.
    (
        "simple_lambda_expression\n"
        "  : attribute_list* modifier* parameter '=>' (block | expression)\n  ;",
        "simple_lambda_expression\n"
        "  : attribute_list* modifier* identifier_token '=>' (block | expression)\n  ;",
    ),
    # (1) allow a bare `;` accessor body.
    (
        "accessor_declaration\n"
        "  : attribute_list* modifier* ('get' | 'set' | 'init' | 'add'"
        " | 'remove' | identifier_token) (block | (arrow_expression_clause"
        " ';'))\n  ;",
        "accessor_declaration\n"
        "  : attribute_list* modifier* ('get' | 'set' | 'init' | 'add'"
        " | 'remove' | identifier_token) (block | (arrow_expression_clause"
        " ';') | ';')\n  ;",
    ),
    # (2) parameter-position modifiers.
    (
        "parameter\n"
        "  : attribute_list* modifier* type? (identifier_token |"
        " '__arglist')? equals_value_clause?\n  ;",
        "parameter\n"
        "  : attribute_list* (modifier | 'out' | 'in' | 'params' | 'this')*"
        " type? (identifier_token | '__arglist')? equals_value_clause?\n  ;",
    ),
    # (3) auto-property initializer. `Syntax.xml` wraps the property body in a
    #     <Choice> of `AccessorList` vs `(ExpressionBody | Initializer)
    #     SemicolonToken`, which the generator renders as ANTLR alternation. But
    #     <Choice> drives the SyntaxFactory overload set and doc comments, not
    #     the hand-written parser: an auto-property may carry an accessor list
    #     AND an initializer (`public bool P { get; } = true;`, C# 6). This is
    #     the one gap where the generator transcribes the model faithfully and
    #     the *model* is stricter than the language.
    (
        "property_declaration\n"
        "  : attribute_list* modifier* type explicit_interface_specifier?"
        " identifier_token (accessor_list | ((arrow_expression_clause |"
        " equals_value_clause) ';'))\n  ;",
        "property_declaration\n"
        "  : attribute_list* modifier* type explicit_interface_specifier?"
        " identifier_token (accessor_list (equals_value_clause ';')? |"
        " ((arrow_expression_clause | equals_value_clause) ';'))\n  ;",
    ),
]

# Roslyn writes every type/enum body as `'{'? member_declaration* '}'?` — both
# braces independently optional — because its parser builds a complete
# declaration node even for unterminated source (`class C {` with no closer).
# That is right for a node model and pathological for a parsing grammar: after
# each member, prediction must weigh "another member of this type" against "the
# type ended without a `}`, so this belongs to the enclosing scope", recursively
# outward. Cost grows ~quadratically in members per type:
#
#     members   as-published   balanced
#          32         2.28 s     0.23 s
#          64         6.55 s     0.21 s
#         128        22.54 s     0.37 s   (61x)
#
# Real files pay it: one 842-line file with 77 members in one class took 125 s,
# and a 1904-line file took 417 s.
#
# Making the braces a *balanced pair* — `('{' member_declaration* '}')?` — keeps
# what the optionality is actually for (a body-less `record R(int X);`) and drops
# only the half-present case. Verified behaviour-identical on the brace-less
# forms (`record R;`, `record R(int X);`, file-scoped namespaces) and on nested
# types, so this is a semantics-preserving rewrite rather than a narrowing.
#
# `.` excludes newlines and every emitted rule body is one line, so a match can
# never span two rules. Step 4c re-checks that rather than leaving it implicit.
BALANCED_BRACES_PATTERN = re.compile(r"LBRACE\? (.*?) RBRACE\?")
BALANCED_BRACES_REPLACEMENT = r"(LBRACE \1 RBRACE)?"

# C# keywords that are *reserved*: never legal as an identifier (ECMA-334 §6.4.4
# "Keywords", excluding the contextual ones listed there separately). Every other
# identifier-shaped literal the grammar mentions is contextual, so it must remain
# usable as a name — see CONTEXTUAL_KEYWORD_NOTE.
RESERVED_KEYWORDS = frozenset(
    """
    abstract as base bool break byte case catch char checked class const continue
    decimal default delegate do double else enum event explicit extern false
    finally fixed float for foreach goto if implicit in int interface internal is
    lock long namespace new null object operator out override params private
    protected public readonly ref return sbyte sealed short sizeof stackalloc
    static string struct switch this throw true try typeof uint ulong unchecked
    unsafe ushort using virtual void volatile while
    """.split()
)

# Literals that look like identifiers but are never names: the UTF-8
# string-literal suffix, which real C# lexes as part of the literal token
# (`"abc"u8`) rather than as a following word. Excluded from the contextual set so
# they do not widen `identifier_token`.
#
# `_` is deliberately NOT excluded here. It is genuinely both the discard
# designation and a legal identifier, and harvesting mints `KW__`, which wins the
# equal-length lexer match over `IDENTIFIER` — so leaving it out made `F(out _)`
# unparsable. That single gap was expensive: the seed error at one `out _` put
# the parser into error recovery, and recovery then accumulated diagnostics
# without bound (>4.29e9 links, 15.5 GB RSS) until it either overflowed the
# runtime's u32 diagnostic arena or the stack. See PROVENANCE.md.
NON_IDENTIFIER_LITERALS = frozenset({"u8", "U8"})

# Roslyn's grammar lists only decimal and hexadecimal integer literals:
#
#     integer_literal_token
#       : decimal_integer_literal_token
#       | hexadecimal_integer_literal_token
#       ;
#
# Binary literals (`0b1010`, C# 7.0) are absent — `Syntax.xml` has a single
# `NumericLiteralToken` kind with no per-base breakdown, so the base-specific
# rules here are prose the generator emitted from the lexical spec, and that
# prose predates C# 7. `BIN_INT_LIT` therefore has to be spliced in by hand;
# without it the token lexes but no parser rule accepts it.
BINARY_LITERAL_FIX = (
    "integer_literal_token\n"
    "  : decimal_integer_literal_token\n"
    "  | hexadecimal_integer_literal_token\n  ;",
    "integer_literal_token\n"
    "  : decimal_integer_literal_token\n"
    "  | hexadecimal_integer_literal_token\n"
    "  | BIN_INT_LIT\n  ;",
)

# `>>` / `>>>` (and their compound assignments) must NOT be single lexer tokens:
# a generic closer and a shift operator are lexically identical, so
# `List<List<int>>` would lex its final `>>` as one right-shift token that
# `type_argument_list`'s `'>'` can never match. This is the classic C#/Java
# angle-bracket ambiguity, and a context-free lexer cannot resolve it.
#
# The remedy is the one `grammars-v4`'s C# grammar uses and that mehen already
# ships patterns for: emit only `'>'`, and re-join the pieces in the *parser*
# behind an adjacency predicate, so `a >> b` is a shift while `List<List<int>>`
# closes two generics. `token_index_adjacent` lowers to pure SemIR — no hooks.
#
# Roslyn itself has no such problem: its published grammar encodes no operator
# precedence at all (that lives in the hand-written parser), so these literals
# only exist here because the harvester minted tokens from `binary_expression`.
SHIFT_TOKEN_RULES = {
    ">>": ("right_shift", "IsRightShift"),
    ">>>": ("unsigned_right_shift", "IsUnsignedRightShift"),
    ">>=": ("right_shift_assignment", "IsRightShiftAssignment"),
    ">>>=": ("unsigned_right_shift_assignment", "IsUnsignedRightShiftAssignment"),
}

# Emitted for each SHIFT_TOKEN_RULES entry: the pieces it is spelled with, in
# lexer-token terms. `>=` stays a single token — it is unambiguous, because a
# generic closer is never followed by `=` in a type position.
#
# `token_index_adjacent` compares only the two most recently consumed tokens
# (`LT(-2).index + 1 == LT(-1).index`), so a three-piece operator needs the
# predicate at *each* junction, not once at the end. Written as
# `'>' '>' {p}? '>' {p}?` so both gaps are checked.
SHIFT_TOKEN_PIECES = {
    ">>": ("'>'", "'>'"),
    ">>>": ("'>'", "'>'", "'>'"),
    ">>=": ("'>'", "'>='"),
    ">>>=": ("'>'", "'>'", "'>='"),
}

# Lowerings for the interpolation state that `lexer-tokens.g4.in` keeps in
# `@lexer::members`. Emitted verbatim into the derived `patterns.toml`; the
# generator matches each `match` against the grammar's literal body text and
# lowers it to SemIR, so no hand-written Rust hook is involved.
#
# The DSL is deliberately tiny: `member`/`member_top`/`member_len`, `not`, int and
# bool literals, `set/add/push/pop_member`, and `seq`. There are no comparisons and
# no `&&`, so each predicate tests one slot's *truthiness* and the conjunction
# comes from rule order in the grammar (deeper case first).
INTERP_MEMBER_PATTERNS = """
# ── interpolated-string state (see lexer-tokens.g4.in) ──────────────────────
#
# `nestDepth` is brace nesting inside the innermost interpolation hole;
# `holeStack` holds one saved depth per enclosing hole, so its own depth answers
# "are we inside a hole at all". Stack-valued member state is runtime 0.20.1+
# (upstream #206).

[[member]]
name = "nestDepth"
kind = "int"
scope = "lexer"

[[member]]
name = "holeStack"
kind = "stack"
scope = "lexer"

# Truthiness only: nonzero means "deeper than the hole's own level".
[[pattern]]
match = "nestDepth > 0"
lower = "not(not(member(nestDepth)))"

# Likewise for the stack's depth: nonzero means a hole is open.
[[pattern]]
match = "holeStack.Count > 0"
lower = "not(not(member_len(holeStack)))"

[[pattern]]
match = "nestDepth++"
lower = "add_member(nestDepth, int(1))"

[[pattern]]
match = "nestDepth--"
lower = "add_member(nestDepth, int(-1))"

# A hole opens: save the enclosing hole's depth, then start this one at 0.
[[pattern]]
match = "holeStack.Push(nestDepth); nestDepth = 0;"
lower = "seq(push_member(holeStack, member(nestDepth)), set_member(nestDepth, int(0)))"

# A hole closes: restore the enclosing depth. `member_top` reads before the pop,
# which is how an assignment-from-pop decomposes in this DSL.
[[pattern]]
match = "nestDepth = holeStack.Pop();"
lower = "seq(set_member(nestDepth, member_top(holeStack)), pop_member(holeStack))"
"""

CONTEXTUAL_KEYWORD_NOTE = """
// A C# *contextual* keyword is recognized only in the position where it has
// meaning and is otherwise an ordinary name: `var`, `record`, `from`, `get`,
// `and`, `required`, … Roslyn's grammar spells each as an inline literal, and
// harvesting those literals into named tokens makes the lexer prefer the keyword
// everywhere (ANTLR breaks an equal-length match by rule order, and the
// harvested tokens precede IDENTIFIER). Widening `identifier_token` to accept
// them back is the standard ANTLR remedy — the same shape `grammars-v4`'s C#
// grammar uses for its `identifier` rule.
//
// Without this, `var x = 1;` fails to parse: `var` is absent from the expected
// token set entirely. The damage is far wider than the keyword itself, because
// `var` appears in most idiomatic modern C# — one wrong token classification
// looks like broken support for raw strings, ranges, `using` declarations, and
// unbound generics all at once.
"""

# Roslyn's "omitted" syntax nodes are genuinely empty productions that model a
# blank slot: `omitted_type_argument` for the unbound generic `Dictionary<,>` and
# `omitted_array_size_expression` for the multi-dimensional `int[,]`. They appear
# as alternatives of `type` and `expression` respectively.
#
# ANTLR cannot have an empty rule inside a closure, so the rules must go — but
# simply deleting their alternatives LOSES REAL SYNTAX: `int[,]` and
# `Dictionary<,>` then fail to parse, because the `','` in
# `'[' (expression (',' expression)*)? ']'` has nothing to match on either side.
# (I shipped that bug once; these two cases are now regression-tested.)
#
# The faithful rewrite makes the list *elements* optional at the two use sites,
# which is exactly what an empty node expressed there.
OMITTED_NODES = ("omitted_type_argument", "omitted_array_size_expression")

OMITTED_USE_SITE_FIXES = [
    # Unbound generic names: `Dictionary<,>`, `List<>`.
    (
        "type_argument_list\n  : '<' (type (',' type)*)? '>'\n  ;",
        "type_argument_list\n  : '<' (type? (',' type?)*)? '>'\n  ;",
    ),
    # Multi-dimensional array ranks: `int[,]`, `int[,,]`.
    (
        "array_rank_specifier\n  : '[' (expression (',' expression)*)? ']'\n  ;",
        "array_rank_specifier\n  : '[' (expression? (',' expression?)*)? ']'\n  ;",
    ),
]


def strip_comments(text: str) -> str:
    """Remove block and line comments (so literals in prose aren't harvested)."""
    text = re.sub(r"/\*.*?\*/", "", text, flags=re.S)
    return re.sub(r"//[^\n]*", "", text)


def rule_span(src: str, name: str) -> re.Match[str] | None:
    return re.search(rf"^{re.escape(name)}\n((?:  [:|].*\n)+)  ;\n", src, re.M)


# The grammar's single entry rule. Declared because the generator otherwise
# treats every top-level rule reaching `EOF` as its own entry, in which case
# nothing can be unreachable.
ENTRY_RULE = "compilation_unit"

# Roslyn's `compilation_unit` does not end in `EOF`, so the parser stops at the
# first thing it cannot continue with and reports success on whatever it consumed.
# `class C { } } } }` parsed with **zero diagnostics** — the stray braces were
# simply never looked at.
#
# That is right for Roslyn's model (its parser reads a compilation unit and the
# caller checks the position) and wrong for a metrics tool, where "parsed cleanly"
# has to mean the whole file was accounted for. Anchoring the entry rule makes the
# unconsumed tail a syntax error, which the diagnostic contract turns into a
# non-zero exit. Both the Java and Kotlin grammars anchor theirs the same way.
ENTRY_RULE_ANCHOR = (
    f"{ENTRY_RULE}\n"
    "  : extern_alias_directive* using_directive* attribute_list* member_declaration*\n"
    "  ;\n"
)
ENTRY_RULE_ANCHORED = (
    f"{ENTRY_RULE}\n"
    "  : extern_alias_directive* using_directive* attribute_list* member_declaration* EOF\n"
    "  ;\n"
)

# Matches the generator's `G4S078` warning line, e.g.
#     warning[G4S078]: CSharpParser.g4:1160:0: parser rule xml_node is
#     unreachable from entry rule compilation_unit
UNREACHABLE_WARNING = re.compile(
    r"^warning\[G4S078\]:.*?: parser rule ([A-Za-z_][A-Za-z_0-9]*) is unreachable\b",
    re.M,
)


def unreachable_rules(src: str, entry: str, generator: str) -> list[str]:
    """Ask the generator which parser rules are unreachable from `entry`.

    The reachability analysis is the generator's (runtime 0.24.0, upstream #262 /
    #264): it walks the real grammar AST, so it distinguishes a rule reference
    from a word inside an action, a label, or an argument list. An earlier
    hand-rolled version here scanned `\\b[a-z_]\\w*\\b` over comment-stripped
    text, which happened to agree on this grammar but is not correct in general —
    and it produced a false positive on Kotlin's `script`.

    The `G4S078` warning is itself the dry run: it needs only `--entry-rule`, not
    `--prune-unreachable`, and runs on the parser grammar alone with no
    `--out-dir`, so this is a ~0.4 s query that writes nothing.
    """
    # ANTLR requires the filename to match the grammar declaration, and at this
    # point in the pipeline the source still carries its upstream name
    # (`grammar csharp;`), so derive the probe filename from the declaration
    # rather than assuming the final one.
    declaration = re.search(r"^(?:lexer |parser )?grammar\s+([A-Za-z_][\w]*)\s*;", src, re.M)
    if not declaration:
        raise RuntimeError("no grammar declaration found for the reachability probe")
    with tempfile.TemporaryDirectory() as tmp:
        probe = Path(tmp) / f"{declaration.group(1)}.g4"
        # `tokenVocab` names a lexer that does not exist yet at this stage; the
        # reachability pass does not need it, and dropping it keeps the probe to
        # a single self-contained file.
        probe.write_text(re.sub(r"^options \{[^}]*\}\n", "", src, flags=re.M))
        result = subprocess.run(
            [generator, probe.name, "--entry-rule", entry],
            cwd=tmp,
            capture_output=True,
            text=True,
            check=False,
        )
    # A hard generator failure here would silently look like "nothing is
    # unreachable", so surface it instead of pruning zero rules.
    if result.returncode != 0 and not UNREACHABLE_WARNING.search(result.stderr):
        raise RuntimeError(
            f"reachability probe failed ({generator} exited {result.returncode}):\n"
            f"{result.stderr.strip()[:2000]}"
        )
    return sorted(set(UNREACHABLE_WARNING.findall(result.stderr)))


def prune_unreachable(src: str, entry: str, generator: str) -> tuple[str, list[str]]:
    """Delete the rules the generator reports unreachable, to a fixpoint.

    Tokenizing the lexical wrapper rules (`decimal_integer_literal_token` →
    `DEC_INT_LIT`) orphans the character-level helpers they used to call
    (`decimal_digit : '0' | '1' | …`, `hexadecimal_digit`, `integer_type_suffix`,
    `identifier_start_character`, …). Those must be removed *before* literals
    are harvested: otherwise their single-character literals become named tokens
    that win equal-length lexer matches, so `'1'` shadows `DEC_INT_LIT` and
    `'a'` shadows `IDENTIFIER` — which silently breaks every parse while the
    grammar still generates cleanly.

    This is why the generator's own `--prune-unreachable` cannot do the job on
    its own: it runs inside codegen, *after* the literals here are harvested, so
    pruning there still emits the junk tokens (259 vs 181) and still mis-lexes.
    The split is analysis vs. edit — the generator decides *which* rules are
    unreachable, this function performs the deletion at the point in the pipeline
    where it has to happen.

    Iterated because removing a rule can orphan helpers only it called; the
    generator reports one round at a time.
    """
    removed: list[str] = []
    while dead := unreachable_rules(src, entry, generator):
        for name in dead:
            if match := rule_span(src, name):
                src = src[: match.start()] + src[match.end() :]
            else:
                raise RuntimeError(f"cannot locate reported unreachable rule {name!r}")
        removed.extend(dead)
    return src, removed


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("source", type=Path, help="upstream CSharp.Generated.g4")
    ap.add_argument("--out-dir", type=Path, default=Path("."))
    ap.add_argument(
        "--generator",
        default=None,
        help=(
            "antlr4-rust-gen as a path or a bare command name, used to compute "
            "rule reachability (default: $MEHEN_ANTLR_RUST_GEN, else PATH)"
        ),
    )
    args = ap.parse_args()

    # The generator owns the reachability analysis (see `unreachable_rules`), so
    # it is a hard requirement rather than an optional accelerator — resolve it
    # up front so a missing tool fails before any work is done.
    #
    # A bare command name is accepted (xtask passes one when the generator came
    # from PATH rather than MEHEN_ANTLR_RUST_GEN), so resolve through `which`
    # before rejecting: a name that is not an existing file may still be a
    # perfectly good executable on PATH.
    requested = args.generator or os.environ.get("MEHEN_ANTLR_RUST_GEN") or "antlr4-rust-gen"
    generator = requested if Path(requested).is_file() else shutil.which(requested)
    if not generator:
        print(
            f"error: {requested!r} not found; pass --generator, set "
            "MEHEN_ANTLR_RUST_GEN, or put antlr4-rust-gen on PATH",
            file=sys.stderr,
        )
        return 1

    src = args.source.read_text(encoding="utf-8-sig")

    # -- 1. Drop the omitted-node rules and their use sites ------------------
    for name in OMITTED_NODES:
        src = re.sub(rf"^{name}\n  : /\* epsilon \*/\n  ;\n\n", "", src, flags=re.M)
    # Preserve the syntax those empty nodes expressed (see OMITTED_USE_SITE_FIXES).
    for old, new in OMITTED_USE_SITE_FIXES:
        if old not in src:
            print(f"error: omitted-node use site not found: {old.splitlines()[0]}", file=sys.stderr)
            return 1
        src = src.replace(old, new, 1)
    kept = [
        line
        for line in src.split("\n")
        if not re.match(rf"^\s*\|\s*({'|'.join(OMITTED_NODES)})\s*$", line)
    ]
    src = "\n".join(kept)
    for name in OMITTED_NODES:
        if name in src:
            print(f"error: {name} still referenced after removal", file=sys.stderr)
            return 1

    # -- 2. Nullability fixes -----------------------------------------------
    for old, new in NULLABILITY_FIXES:
        if old not in src:
            print(f"error: nullability target not found: {old.splitlines()[0]}", file=sys.stderr)
            return 1
        src = src.replace(old, new)

    # -- 2b. Restore the `record` contextual keyword -------------------------
    if RECORD_KEYWORD_TARGET not in src:
        print("error: record_declaration shape changed upstream", file=sys.stderr)
        return 1
    src = src.replace(RECORD_KEYWORD_TARGET, RECORD_KEYWORD_REPLACEMENT, 1)
    src = src.rstrip() + "\n" + RECORD_KEYWORD_RULE

    # -- 2c. Repair generator blind spots (see GENERATOR_GAP_FIXES) ----------
    for old, new in GENERATOR_GAP_FIXES:
        if old not in src:
            print(
                f"error: generator-gap target not found: {old.splitlines()[0]}",
                file=sys.stderr,
            )
            return 1
        src = src.replace(old, new, 1)

    # -- 2d. Split the shift operators off the generic closer -----------------
    # Each `'>>'`-family literal becomes a rule that spells the operator out of
    # `'>'` pieces behind an adjacency predicate (see SHIFT_TOKEN_RULES).
    shift_rules: list[str] = []
    for literal, (rule, helper) in SHIFT_TOKEN_RULES.items():
        quoted = f"'{literal}'"
        if quoted not in src:
            print(f"error: shift literal {quoted} not found", file=sys.stderr)
            return 1
        # The predicate goes after every piece but the first, so each junction is
        # checked (it only ever compares the last two consumed tokens).
        head, *rest = SHIFT_TOKEN_PIECES[literal]
        body = head + "".join(f" {piece} {{this.{helper}()}}?" for piece in rest)
        shift_rules.append(f"\n{rule}\n  : {body} // adjacent in the char stream?\n  ;\n")
    for literal in sorted(SHIFT_TOKEN_RULES, key=len, reverse=True):
        src = src.replace(f"'{literal}'", SHIFT_TOKEN_RULES[literal][0])
    src = src.rstrip() + "\n" + "".join(shift_rules)

    # -- 2e. Accept binary integer literals ----------------------------------
    old, new = BINARY_LITERAL_FIX
    if old not in src:
        print("error: integer_literal_token shape changed upstream", file=sys.stderr)
        return 1
    src = src.replace(old, new, 1)

    # -- 2e2. Let members be bodiless ----------------------------------------
    # See BODY_REQUIRING_RULES: an abstract / interface / extern / partial member
    # has only a semicolon, and without this it falls through to
    # `global_statement` and parses as a call expression.
    for rule in BODY_REQUIRING_RULES:
        match = rule_span(src, rule)
        if not match:
            print(f"error: {rule} not found", file=sys.stderr)
            return 1
        body = match.group(1)
        if REQUIRED_BODY not in body:
            print(
                f"error: {rule} body shape changed upstream (no {REQUIRED_BODY!r})",
                file=sys.stderr,
            )
            return 1
        src = src.replace(
            match.group(0),
            f"{rule}\n{body.replace(REQUIRED_BODY, OPTIONAL_BODY, 1)}  ;\n",
            1,
        )

    # -- 2f. Deprioritize declaration_expression -----------------------------
    # See DECLARATION_EXPRESSION_ALT: it shadows every invocation otherwise.
    expression_rule = rule_span(src, "expression")
    if not expression_rule:
        print("error: expression rule not found", file=sys.stderr)
        return 1
    body = expression_rule.group(1)
    if DECLARATION_EXPRESSION_ALT not in body:
        print(
            "error: declaration_expression is not an alternative of `expression`",
            file=sys.stderr,
        )
        return 1
    reordered = body.replace(DECLARATION_EXPRESSION_ALT, "") + DECLARATION_EXPRESSION_ALT
    src = src.replace(expression_rule.group(0), f"expression\n{reordered}  ;\n", 1)

    # -- 2g. Prioritize record_declaration in member position ----------------
    # See RECORD_DECL_ALT: `record R(int X);` parses as a phantom *method* otherwise,
    # because `record` is widened back into `identifier_token` and so is a viable
    # return type. Safe only because `record_keyword` is a real KW_RECORD token now.
    member_rule = rule_span(src, "member_declaration")
    if not member_rule:
        print("error: member_declaration rule not found", file=sys.stderr)
        return 1
    body = member_rule.group(1)
    if MEMBER_METHOD_ALT not in body:
        print(
            "error: base_method_declaration is not an alternative of `member_declaration`",
            file=sys.stderr,
        )
        return 1
    if RECORD_DECL_ALT in body:
        print(
            "error: record_declaration is already a member_declaration alternative",
            file=sys.stderr,
        )
        return 1
    src = src.replace(
        member_rule.group(0),
        "member_declaration\n"
        + body.replace(MEMBER_METHOD_ALT, RECORD_DECL_ALT + MEMBER_METHOD_ALT, 1)
        + "  ;\n",
        1,
    )
    print("hoisted record_declaration ahead of base_method_declaration")

    # -- 2h. Drop the error-recovery member alternative ----------------------
    # See INCOMPLETE_MEMBER_ALT: it makes `class C { int }` an error-free parse.
    member_rule = rule_span(src, "member_declaration")
    if not member_rule or INCOMPLETE_MEMBER_ALT not in member_rule.group(1):
        print(
            "error: incomplete_member is not an alternative of `member_declaration`",
            file=sys.stderr,
        )
        return 1
    src = src.replace(
        member_rule.group(0),
        "member_declaration\n"
        + member_rule.group(1).replace(INCOMPLETE_MEMBER_ALT, "", 1)
        + "  ;\n",
        1,
    )
    print("dropped the incomplete_member recovery alternative")

    # -- 3. Point character-level rules at real lexer tokens -----------------
    lexer_bound = dict(LEXER_TOKEN_RULES)
    lexer_bound.update(
        (rule, INTERP_RAW_START_TOKEN) for rule in INTERP_RAW_START_RULES
    )
    for rule, token in lexer_bound.items():
        m = rule_span(src, rule)
        if not m:
            print(f"error: lexer-bound rule not found: {rule}", file=sys.stderr)
            return 1
        src = src[: m.start()] + f"{rule}\n  : {token}\n  ;\n" + src[m.end() :]

    # -- 3b. Prune rules the tokenization orphaned ---------------------------
    try:
        src, pruned = prune_unreachable(src, ENTRY_RULE, generator)
    except RuntimeError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    if pruned:
        print(f"pruned {len(pruned)} unreachable rules: {', '.join(sorted(pruned))}")

    # -- 3c. Anchor the entry rule at EOF ------------------------------------
    # See ENTRY_RULE_ANCHOR. Deliberately AFTER pruning: the generator treats every
    # top-level rule that reaches `EOF` as an entry point, so anchoring first would
    # make nothing unreachable and the 84 orphaned helpers would survive.
    if ENTRY_RULE_ANCHOR not in src:
        print(
            f"error: {ENTRY_RULE} not in the expected unanchored form",
            file=sys.stderr,
        )
        return 1
    src = src.replace(ENTRY_RULE_ANCHOR, ENTRY_RULE_ANCHORED, 1)
    print(f"anchored {ENTRY_RULE} at EOF")

    # -- 4. Harvest the remaining inline literals into named tokens ----------
    # A combined grammar would let ANTLR synthesize implicit tokens for these,
    # but a split pair needs them named so `tokenVocab` can carry them.
    body = strip_comments(src)
    literals = sorted(
        {
            lit
            for lit in re.findall(r"'((?:[^'\\\n]|\\.)*)'", body)
            # Interpolation delimiters are mode-switching named tokens, not
            # harvested literals (see INTERP_TOKEN_LITERALS).
            if lit and lit not in INTERP_TOKEN_LITERALS
        },
        key=lambda s: (-len(s), s),
    )
    names: dict[str, str] = {}
    unnamed: list[str] = []
    for index, lit in enumerate(literals):
        if lit in STABLE_TOKEN_NAMES:
            names[lit] = STABLE_TOKEN_NAMES[lit]
        elif re.fullmatch(r"[a-zA-Z_][a-zA-Z_0-9]*", lit):
            names[lit] = f"KW_{lit.upper()}"
        else:
            # Reached only for an operator STABLE_TOKEN_NAMES does not cover.
            # Named by index so generation can continue and the report below can
            # list every gap at once, then rejected.
            names[lit] = f"OP_{index:03d}"
            unnamed.append(lit)
    if unnamed:
        # Index names are position-derived, so they rebind on any upstream
        # literal change. Failing here keeps that from reaching hand-written
        # code that names tokens (the walker, `lexer-tokens.g4.in`).
        print(
            "error: operator literals missing from STABLE_TOKEN_NAMES: "
            + ", ".join(repr(lit) for lit in unnamed),
            file=sys.stderr,
        )
        return 1
    # Keyword names can collide when the grammar spells the same word in two
    # cases (`U8` and `u8` both want `KW_U8`). Disambiguate from the literal's own
    # spelling, NOT from its index: an index suffix is exactly the position-derived
    # naming rejected above, so `KW_U8_150` would rebind to a different literal the
    # moment upstream adds or removes one. `KW_U8` / `KW_U8_LOWER` are stable as
    # long as the two spellings are.
    #
    # The uppercase spelling keeps the bare name (it is what `KW_{lit.upper()}`
    # already produces), so only the lowercase variant is suffixed; a collision
    # between anything other than a pure case difference is a real ambiguity and
    # fails the assertion below.
    seen: dict[str, str] = {}
    for lit in literals:
        name = names[lit]
        if name in seen:
            names[lit] = f"{name}_LOWER" if lit.islower() else f"{name}_UPPER"
        seen[names[lit]] = lit
    assert len(set(names.values())) == len(literals), "token-name collision"

    for lit in literals:  # longest-first so `>>=` is not clobbered by `>`
        src = src.replace(f"'{lit}'", names[lit])
    # Longest-first here too: `$@"` must be replaced before `$"`.
    for lit in sorted(INTERP_TOKEN_LITERALS, key=len, reverse=True):
        src = src.replace(f"'{lit}'", INTERP_TOKEN_LITERALS[lit])

    # -- 4b. Let contextual keywords be identifiers again --------------------
    # Runs after harvesting because it needs the generated token names.
    contextual = sorted(
        lit
        for lit in literals
        if re.fullmatch(r"[a-zA-Z_][a-zA-Z_0-9]*", lit)
        and lit not in RESERVED_KEYWORDS
        and lit not in NON_IDENTIFIER_LITERALS
        and not lit.startswith("__")  # `__arglist` &c. are reserved compiler-isms
    )
    if not contextual:
        print("error: no contextual keywords found to widen", file=sys.stderr)
        return 1
    # The minted `KW_RECORD` is contextual too, and it is not among the harvested
    # literals (Roslyn's grammar never spells it), so it is added by name.
    contextual_tokens = [names[lit] for lit in contextual] + [RECORD_TOKEN_NAME]
    identifier_alts = "\n".join(f"  | {token}" for token in contextual_tokens)
    old_identifier = f"identifier_token\n  : {LEXER_TOKEN_RULES['identifier_token']}\n  ;"
    if old_identifier not in src:
        print("error: identifier_token not in expected tokenized form", file=sys.stderr)
        return 1
    src = src.replace(
        old_identifier,
        f"{CONTEXTUAL_KEYWORD_NOTE.strip()}\nidentifier_token\n"
        f"  : {LEXER_TOKEN_RULES['identifier_token']}\n{identifier_alts}\n  ;",
        1,
    )
    print(f"widened identifier_token with {len(contextual_tokens)} contextual keywords")

    # -- 4b2. Keep a pattern combinator out of a variable designation ---------
    # See COMBINATOR_KEYWORDS. Spelled as the full contextual set minus the three
    # combinators, rather than as `identifier_token` with exclusions, because ANTLR
    # has no rule-level token subtraction.
    if DESIGNATION_RULE not in src:
        print(
            "error: single_variable_designation not in expected tokenized form",
            file=sys.stderr,
        )
        return 1
    missing = [kw for kw in COMBINATOR_KEYWORDS if kw not in names]
    if missing:
        print(
            "error: pattern combinators absent from the harvested literals: "
            + ", ".join(missing),
            file=sys.stderr,
        )
        return 1
    excluded = {names[kw] for kw in COMBINATOR_KEYWORDS}
    designation_alts = "\n".join(
        f"  | {token}" for token in contextual_tokens if token not in excluded
    )
    src = src.replace(
        DESIGNATION_RULE,
        "// A pattern combinator (`and`/`or`/`not`) is excluded: it is a contextual\n"
        "// keyword, so widening `identifier_token` would let `o is int and > 5` bind\n"
        "// `and` as a variable name and silently drop the combinator. See\n"
        "// COMBINATOR_KEYWORDS in prepare-grammar.py.\n"
        "single_variable_designation\n"
        f"  : {LEXER_TOKEN_RULES['identifier_token']}\n{designation_alts}\n  ;",
        1,
    )
    print(f"narrowed single_variable_designation (excluded {', '.join(COMBINATOR_KEYWORDS)})")

    # -- 4c. Pair up the type-body braces ------------------------------------
    # After harvesting, so the brace tokens have their STABLE_TOKEN_NAMES names.
    for match in BALANCED_BRACES_PATTERN.finditer(src):
        if "\n" in match.group(0):
            print("error: brace-balancing match spans two rules", file=sys.stderr)
            return 1
    src, braced = BALANCED_BRACES_PATTERN.subn(BALANCED_BRACES_REPLACEMENT, src)
    if not braced:
        print("error: no optional-brace bodies found to balance", file=sys.stderr)
        return 1
    print(f"balanced the brace pair in {braced} body rules")

    # -- 5. Emit the parser grammar -----------------------------------------
    src = re.sub(r"^//[^\n]*\n", "", src)  # drop the auto-generated banner
    src = re.sub(r"^grammar csharp;\n", "", src, flags=re.M)
    parser = (
        "// @generated from Roslyn's CSharp.Generated.g4 by "
        "prepare-roslyn-grammar.py — do not hand-edit.\n"
        "// See PROVENANCE.md for the pinned upstream revision and the patch rationale.\n"
        "parser grammar CSharpParser;\n\n"
        "options { tokenVocab=CSharpLexer; }\n" + src
    )
    (args.out_dir / "CSharpParser.g4").write_text(parser)

    # -- 6. Emit the lexer grammar ------------------------------------------
    # Keyword/operator/punctuation tokens come first: ANTLR breaks an
    # equal-length match by rule order, so `KW_CLASS` must precede `IDENTIFIER`
    # or every keyword would lex as an identifier.
    def hand_written(name: str) -> Path | None:
        """Locate a hand-written `.g4.in` beside the source, else beside this file."""
        candidate = args.source.parent / name
        return candidate if candidate.is_file() else Path(__file__).with_name(name)

    token_rules = hand_written(LEXER_RULES_FILE)
    lexer_members = hand_written(LEXER_MEMBERS_FILE)
    for required in (token_rules, lexer_members):
        if not required.is_file():
            print(f"error: missing {required.name}", file=sys.stderr)
            return 1
    mode_tokens = sorted(
        token for rule, token in LEXER_TOKEN_RULES.items()
        if rule in MODE_SCOPED_RULES
    )
    lexer = "\n".join(
        [
            "// @generated from Roslyn's CSharp.Generated.g4 by "
            "prepare-grammar.py — do not hand-edit.",
            "// Roslyn publishes a parser-only grammar; this lexer supplies the",
            "// terminals it references. Literal tokens below are harvested from the",
            "// parser's inline literals; the rest is spliced from "
            f"`{LEXER_RULES_FILE}` and `{LEXER_MEMBERS_FILE}`.",
            "// See PROVENANCE.md.",
            "lexer grammar CSharpLexer;",
            "",
            "channels { COMMENTS_CHANNEL, DIRECTIVE }",
            "",
            "// Emitted only from their lexer modes, but referenced by the parser,",
            "// so they must be declared up front.",
            "tokens { " + ", ".join(mode_tokens) + " }",
            "",
            # ANTLR requires named actions in the header, before any rule, so the
            # `@lexer::members` block is a separate file from the rules.
            lexer_members.read_text().rstrip(),
            "",
            "// ---- keywords, operators, punctuation (must precede IDENTIFIER) ----",
        ]
        # `{`, `}` and `:` are omitted here and defined by `lexer-tokens.g4.in`
        # instead: inside an interpolation hole they need predicate-gated rules
        # that must *precede* the plain literal, and ANTLR breaks an equal-length
        # match by rule order. Everything else keeps the literals-first ordering,
        # which is what makes `KW_CLASS` beat `IDENTIFIER`.
        + [
            f"{names[lit]} : '{lit}' ;"
            for lit in literals
            if lit not in HOLE_SENSITIVE_LITERALS
        ]
        # `record` is the one keyword Roslyn's grammar never spells as a literal, so
        # it cannot be harvested; it is minted here instead. Placed with the
        # harvested keywords so it precedes IDENTIFIER, and widened back into
        # `identifier_token` above so it stays a legal name.
        + [RECORD_TOKEN_RULE]
        + [
            "",
            # No substitution needed: the tokens `lexer-tokens.g4.in` refers to
            # by name are the ones STABLE_TOKEN_NAMES pins.
            token_rules.read_text().rstrip(),
        ]
    )
    (args.out_dir / "CSharpLexer.g4").write_text(lexer + "\n")

    # -- 7. Emit the semantic-pattern file ----------------------------------
    shift_helpers = "".join(
        "\n[[helper]]\n"
        'kind = "parser-predicate"\n'
        f'name = "{helper}"\n'
        'returns = "bool"\n'
        'lower = "token_index_adjacent"\n'
        for _rule, helper in SHIFT_TOKEN_RULES.values()
    )
    (args.out_dir / "patterns.toml").write_text(
        "version = 1\n\n"
        "# `record` needs no helper here: Roslyn declares\n"
        "# `<ContextualKind Name=\"RecordKeyword\"/>` on\n"
        "# RecordDeclarationSyntax.Keyword, but its grammar generator reads only\n"
        "# `<Kind>`, so the published grammar spells the keyword as the catch-all\n"
        "# `syntax_token`. `prepare-grammar.py` restores the restriction by minting a\n"
        "# real `KW_RECORD` token (see RECORD_KEYWORD_RULE) rather than by predicating\n"
        "# over `IDENTIFIER` — a predicate cannot prune a path ANTLR has already\n"
        "# committed to, and this position overlaps several member forms.\n\n"
        "# `>>` / `>>>` (and their compound assignments) are spelled as adjacent\n"
        "# `>` tokens so a generic closer never lexes as a shift operator (see\n"
        "# SHIFT_TOKEN_RULES). Each predicate checks the pieces were adjacent in the\n"
        "# char stream, so `a >> b` shifts while `List<List<int>>` closes two\n"
        "# generics. Same lowering the vendored grammars-v4 grammar uses.\n"
        + shift_helpers
        + INTERP_MEMBER_PATTERNS
    )

    print(f"wrote CSharpParser.g4, CSharpLexer.g4, patterns.toml ({len(literals)} literal tokens)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
