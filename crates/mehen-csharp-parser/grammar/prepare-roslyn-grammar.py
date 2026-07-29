#!/usr/bin/env python3
"""Derive a generatable ANTLR grammar pair from Roslyn's published C# grammar.

`dotnet/roslyn` publishes `CSharp.Generated.g4`, machine-generated from
`Syntax.xml` — the same model that generates the compiler's own syntax nodes.
It therefore tracks C# *as implemented*, which no community grammar does. But
it is a **reference** grammar, not a working parser, and needs three
mechanical corrections before ANTLR (or `antlr4-rust-gen`) will accept it:

1.  **Empty rules.** Two `/* epsilon */` rules model Roslyn's *omitted* syntax
    nodes (the blank slots in `Foo<,>` / `new int[,]`), and four
    `/* see lexical specification */` rules are token stubs. ANTLR rejects an
    empty rule inside a closure (`error(153)`), and these propagate to 23
    errors across `compilation_unit`, every type declaration, and the XML
    doc-comment rules.

2.  **No lexer.** The grammar is parser-only: terminals are inline literals
    plus character-level rules (`decimal_digit : '0' | '1' | …`,
    `identifier_token : '@'? identifier_start_character …`). Those must move
    into a real lexer, or single-character tokens shadow multi-character ones
    (`'C'` beats `IDENTIFIER`, `'1'` beats a decimal literal). Two tokens are
    also only valid inside a lexer *mode*: interpolated-string text and XML
    doc-comment text.

3.  **Nullable closure members.** `incomplete_member` (Roslyn's
    error-recovery node) and the XML text/trivia wrappers are all-optional, so
    they make `member_declaration*` and friends nullable.

This script is the single source of that transform: run it to regenerate the
vendored `CSharpLexer.g4` / `CSharpParser.g4` from a pinned upstream revision,
so the patch set is reproducible and reviewable rather than hand-edited. See
`PROVENANCE.md` for the pinned commit and the rationale.

Status: NOT YET WIRED IN — the parser works; the analyzer walker does not exist
yet for this grammar shape.

Verified on runtime/generator 0.21.0: generation is clean under
`--sem-unknown error --require-full-semantics`, the parser compiles, and 19/19
modern-C# probes parse — records, `is not`, `and`/`or`/relational patterns, list
patterns, collection expressions, file-scoped namespaces, `??=`, nullable refs,
switch expressions, and all five interpolated-string shapes (simple, nested
braces, escaped braces, format clause, nested interpolated string, verbatim).

Interpolation needs three lexer modes plus a typed hook that owns the mode
transitions (`hooks-interpolation.rs.in`), because the `}` closing a hole is
lexically identical to one closing a nested block. That is deliberate: mehen is
the demonstrative consumer of antlr-rust-runtime, so using its hook and
`--sem-patterns` surfaces to express real grammar power is the point, not a cost.

On 321 real `System.Text.Json` files: 115 parse clean (vs 93 for the vendored
C#7 grammar) in ~4m. 206 still report errors and 25 files exceed 2s, so the
remaining work is characterizing those — the causes are not yet known, and past
experience on this grammar says measure before concluding.

Remaining to switch the analyzer over: rewrite `mehen-csharp`'s walker for this
grammar's rule names, accounting for #221 inlining 16 of 17 LR-cycle satellites
(invocation/member_access/assignment/conditional all collapse into
RULE_EXPRESSION and must be classified by token probe), then re-map the 85
existing C# metric tests.

Usage:
    python3 prepare-roslyn-grammar.py CSharp.Generated.g4 --out-dir .
"""

from __future__ import annotations

import argparse
import re
import sys
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

# The subset of LEXER_TOKEN_RULES whose tokens are produced from a lexer mode
# rather than the default mode, so they need a `tokens {}` declaration.
MODE_SCOPED_RULES = ("interpolated_string_text_token", "xml_text_literal_token")

# Rules that are all-optional and so make their closure-using callers nullable.
# Each is tightened to require at least one element. `incomplete_member` is
# Roslyn's error-recovery node (it exists to model broken source) and the XML
# wrappers are `x*` over a nullable element.
NULLABILITY_FIXES = [
    (
        "incomplete_member\n  : attribute_list* modifier* type?\n  ;",
        "incomplete_member\n  : attribute_list* modifier* type\n  ;",
    ),
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

// Contextual keyword: `record` lexes as an ordinary IDENTIFIER (it is legal as a
// name), so the declaration position is restricted by a predicate on the token
// text. This restores Roslyn's <ContextualKind Name="RecordKeyword"/>, which its
// grammar generator drops. Lowered by `patterns.toml` to a pure SemIR
// comparison, so no hooks are needed.
record_keyword
  : {this.IsRecordKeyword()}? identifier_token
  ;
"""

# Interpolated strings need lexer modes, so the tokens that delimit them cannot
# be plain harvested literals. These literals are therefore NOT harvested; the
# parser is rewritten to reference the named, mode-switching tokens that
# `lexer-tokens.g4.in` defines instead.
#
# `'{'` and `'}'` stay ordinary harvested literals — they are ordinary C# braces
# almost everywhere. The interpolation-hole bookkeeping is done by the typed
# hook in `src/hooks.rs`, which watches accepted tokens rather than by giving
# the brace tokens grammar actions (a `}` cannot know from the grammar alone
# whether it closes a hole or a nested block).
INTERP_TOKEN_LITERALS = {
    '$"': "INTERP_START",
    '$@"': "INTERP_VERBATIM_START",
}

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


def prune_unreachable(src: str, entry: str = "compilation_unit") -> tuple[str, list[str]]:
    """Drop rules unreachable from `entry`, repeatedly until a fixpoint.

    Tokenizing the lexical wrapper rules (`decimal_integer_literal_token` →
    `DEC_INT_LIT`) orphans the character-level helpers they used to call
    (`decimal_digit : '0' | '1' | …`, `hexadecimal_digit`, `integer_type_suffix`,
    `identifier_start_character`, …). Those must be removed *before* literals
    are harvested: otherwise their single-character literals become named tokens
    that win equal-length lexer matches, so `'1'` shadows `DEC_INT_LIT` and
    `'a'` shadows `IDENTIFIER` — which silently breaks every parse while the
    grammar still generates cleanly.
    """
    removed: list[str] = []
    while True:
        rules = {
            m.group(1): m.group(2)
            for m in re.finditer(r"^([a-z_][a-zA-Z_0-9]*)\n((?:  [:|].*\n)+)  ;\n", src, re.M)
        }
        reachable = {entry}
        frontier = [entry]
        while frontier:
            body = rules.get(frontier.pop(), "")
            for name in re.findall(r"\b([a-z_][a-zA-Z_0-9]*)\b", strip_comments(body)):
                if name in rules and name not in reachable:
                    reachable.add(name)
                    frontier.append(name)
        dead = [name for name in rules if name not in reachable]
        if not dead:
            return src, removed
        for name in dead:
            m = rule_span(src, name)
            if m:
                src = src[: m.start()] + src[m.end() :]
        removed.extend(dead)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("source", type=Path, help="upstream CSharp.Generated.g4")
    ap.add_argument("--out-dir", type=Path, default=Path("."))
    args = ap.parse_args()

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

    # -- 3. Point character-level rules at real lexer tokens -----------------
    for rule, token in LEXER_TOKEN_RULES.items():
        m = rule_span(src, rule)
        if not m:
            print(f"error: lexer-bound rule not found: {rule}", file=sys.stderr)
            return 1
        src = src[: m.start()] + f"{rule}\n  : {token}\n  ;\n" + src[m.end() :]

    # -- 3b. Prune rules the tokenization orphaned ---------------------------
    src, pruned = prune_unreachable(src)
    if pruned:
        print(f"pruned {len(pruned)} unreachable rules: {', '.join(sorted(pruned))}")

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
    for index, lit in enumerate(literals):
        if re.fullmatch(r"[a-zA-Z_][a-zA-Z_0-9]*", lit):
            names[lit] = f"KW_{lit.upper()}"
        else:
            names[lit] = f"OP_{index:03d}"
    # Keyword names can collide when the grammar spells the same word in two
    # cases (`U8`/`u8`); disambiguate deterministically by index.
    seen: dict[str, str] = {}
    for lit in literals:
        name = names[lit]
        if name in seen:
            names[lit] = f"{name}_{literals.index(lit)}"
        seen[names[lit]] = lit
    assert len(set(names.values())) == len(literals), "token-name collision"

    for lit in literals:  # longest-first so `>>=` is not clobbered by `>`
        src = src.replace(f"'{lit}'", names[lit])
    # Longest-first here too: `$@"` must be replaced before `$"`.
    for lit in sorted(INTERP_TOKEN_LITERALS, key=len, reverse=True):
        src = src.replace(f"'{lit}'", INTERP_TOKEN_LITERALS[lit])

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
    token_rules = (args.source.parent / LEXER_RULES_FILE)
    if not token_rules.is_file():
        token_rules = Path(__file__).with_name(LEXER_RULES_FILE)
    if not token_rules.is_file():
        print(f"error: missing {LEXER_RULES_FILE}", file=sys.stderr)
        return 1
    mode_tokens = sorted(
        token for rule, token in LEXER_TOKEN_RULES.items()
        if rule in MODE_SCOPED_RULES
    )
    lexer = "\n".join(
        [
            "// @generated from Roslyn's CSharp.Generated.g4 by "
            "prepare-roslyn-grammar.py — do not hand-edit.",
            "// Roslyn publishes a parser-only grammar; this lexer supplies the",
            "// terminals it references. Literal tokens below are harvested from the",
            "// parser's inline literals; the rest is spliced from "
            f"`{LEXER_RULES_FILE}`.",
            "// See PROVENANCE.md.",
            "lexer grammar CSharpLexer;",
            "",
            "channels { COMMENTS_CHANNEL, DIRECTIVE }",
            "",
            "// Emitted only from their lexer modes, but referenced by the parser,",
            "// so they must be declared up front.",
            "tokens { " + ", ".join(mode_tokens) + " }",
            "",
            "// ---- keywords, operators, punctuation (must precede IDENTIFIER) ----",
        ]
        + [f"{names[lit]} : '{lit}' ;" for lit in literals]
        + [
            "",
            # `lexer-tokens.g4.in` refers to the harvested `{` and `"` tokens
            # by placeholder, because their generated names are index-based and
            # shift whenever the upstream grammar's literal set changes.
            token_rules.read_text()
            .rstrip()
            .replace("@LBRACE@", names["{"])
            .replace("@DQUOTE@", names['"'])
            .replace("@RBRACE@", names["}"]),
        ]
    )
    (args.out_dir / "CSharpLexer.g4").write_text(lexer + "\n")

    # -- 7. Emit the semantic-pattern file ----------------------------------
    (args.out_dir / "patterns.toml").write_text(
        "version = 1\n\n"
        "# `record` is a contextual keyword. Roslyn declares\n"
        "# `<ContextualKind Name=\"RecordKeyword\"/>` on\n"
        "# RecordDeclarationSyntax.Keyword, but its grammar generator reads only\n"
        "# `<Kind>`, so the published grammar spells the keyword as the catch-all\n"
        "# `syntax_token` — which makes `class` viable as a record declaration and\n"
        "# costs ~quadratic prediction time per type member. `prepare-roslyn-grammar.py`\n"
        "# restores the restriction as a text comparison on the lookahead token; it\n"
        "# lowers to a pure SemIR expression, so no typed hook is needed.\n\n"
        "[[helper]]\n"
        'kind = "parser-predicate"\n'
        'name = "IsRecordKeyword"\n'
        'returns = "bool"\n'
        'lower = "cmp(eq, token_text(1), str(\\"record\\"))"\n'
    )

    print(f"wrote CSharpParser.g4, CSharpLexer.g4, patterns.toml ({len(literals)} literal tokens)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
