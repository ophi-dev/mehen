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

Status: NOT YET IN USE — the prep is close but not complete.

Working: generation under `antlr4-rust-gen` 0.21.0 is clean (mutual left
recursion accepted per upstream #221), the parser compiles, and 13/13
modern-C# probes parse (records, `is not`, `and`/`or`/relational patterns,
list patterns, collection expressions, file-scoped namespaces, `??=`,
nullable refs, switch expressions). On 321 real files from `dotnet/runtime`'s
`System.Text.Json`, 109 parse clean versus 93 for the vendored C#7 grammar.

Remaining gap: interpolated strings. The INTERPOLATION lexer mode below is
never entered, because `$"` is harvested as an ordinary literal token rather
than a mode-pushing one, so any file using `$"...{expr}..."` fails. That is
the main reason 212 files still report errors.

Also note the performance finding recorded in PROVENANCE.md: Roslyn's
optional body braces (`'{'? member_declaration* '}'?`, present for error
recovery) make member boundaries ambiguous and cost O(n^2) in members per
type — 18 members took 6.5s, and one 953-line file took 272s. Requiring the
braces makes it flat (18 members in 0.07s, ~93x faster) and the whole corpus
runs in 61s instead of over 600s. That patch is NOT yet applied here.
Filed upstream as antlr-rust-runtime#248, with a one-command reproduction in
`repro/roslyn-csharp-perf/` at the repo root.

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

# Roslyn's "omitted" syntax nodes: genuinely empty productions that model a
# blank slot in `Foo<,>` and `new int[,]`. ANTLR cannot have an empty rule
# inside a closure, so the rule is dropped and its use sites — which are
# alternatives of an alternation — are removed, leaving the surrounding
# optional/closure to express the same language.
OMITTED_NODES = ("omitted_type_argument", "omitted_array_size_expression")


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
        {lit for lit in re.findall(r"'((?:[^'\\\n]|\\.)*)'", body) if lit},
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
        + ["", token_rules.read_text().rstrip()]
    )
    (args.out_dir / "CSharpLexer.g4").write_text(lexer + "\n")

    print(f"wrote CSharpParser.g4 and CSharpLexer.g4 ({len(literals)} literal tokens)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
