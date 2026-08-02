# C# ANTLR grammar — provenance

The **source of truth** is the single vendored file `CSharp.Generated.g4`, taken
verbatim from `dotnet/roslyn`. Everything else in this directory is either input
to the transform or produced by it:

| File | Role |
|---|---|
| `CSharp.Generated.g4` | vendored upstream grammar (see Source) |
| `lexer-tokens.g4.in` | hand-written lexer rules — Roslyn publishes no lexer |
| `lexer-members.g4.in` | the lexer's `@lexer::members` state; separate because ANTLR requires named actions in the header, before any rule |
| `prepare-grammar.py` | the transform; a step of parser generation |
| `CSharpLexer.g4`, `CSharpParser.g4`, `patterns.toml` | **derived** into a process-local scratch dir, gitignored here |

`cargo run -p xtask -- antlr generate csharp` runs the transform and then
`antlr4-rust-gen`, writing the Rust modules in `../src/generated/`. That needs
[`uv`](https://docs.astral.sh/uv/) in addition to the generator; the script's PEP
723 block pins the interpreter.

The derived pair goes to a **process-local scratch directory**, and the generator
runs there — not in this tree. Two xtask invocations in one checkout (a developer
alongside CI, say) would otherwise each truncate and rewrite the same derived files
while the other's generator was reading them. To inspect the derived grammar, run the
script by hand: `uv run prepare-grammar.py CSharp.Generated.g4 --out-dir .`, which is
also how you iterate on the transform.

## Source

| Field | Value |
|---|---|
| Upstream | [`dotnet/roslyn`](https://github.com/dotnet/roslyn) — the C# compiler itself |
| Path | `src/Compilers/CSharp/Portable/Generated/CSharp.Generated.g4` |
| Branch | `main` |
| Repo revision | `c9f12709e0cd477febd54d1a5b5e3e3731a1ada2` (2026-07-31) |
| File last changed | `76234ec6a1ba46f05b8b07dbeaeb7e39c5054810` (2026-06-24) |

The grammar is machine-generated from `Syntax.xml`, the same model that generates
the compiler's own syntax nodes, so it tracks **C# as implemented** — records,
`is not`, `and`/`or`/relational patterns, list patterns, collection expressions,
raw strings, primary constructors, `required` members. No community grammar does.

It is also a *reference* grammar rather than a working parser, which is what
`prepare-grammar.py` exists to fix; the transform's repairs are catalogued below.

### Why not `antlr/grammars-v4`

This crate previously vendored `csharp/v7/{CSharpLexer,CSharpParser}.g4` from
`antlr/grammars-v4`. It is a genuine C# 7-era grammar, so mainstream post-7
syntax simply does not parse — `switch` expressions, `is not`, `and`/`or`
patterns, and `record` declarations all fail, and C# 9 pattern syntax appears
throughout modern .NET. On the 322-file `System.Text.Json` corpus it parsed 93
files cleanly versus 318 for the derived Roslyn grammar.

Upstream's `csharp/v8-spec` is not the answer either: it stops at C# 8, so it
still lacks the C# 9 patterns that cause most failures, and its `superClass`
surface is ~35 helpers including a **symbol-table-driven scope stack**
(`EnterTypeScope`, `IsClassTypeName`, `IsTypeParameterName`, …). Those resolve
identifiers against declarations seen so far, so porting them means implementing
a semantic model rather than lookahead checks — and mehen's `--sem-unknown error`
policy requires every one to be exact or generation fails.

Note also that the grammar is **permissive by design** — it models Roslyn's
syntax nodes (including error-recovery nodes such as `incomplete_member`), not
the exact accepted language — and it encodes **no operator precedence**, since
real C# precedence lives in Roslyn's hand-written parser. Neither matters for
mehen's token-level metrics, but both would matter for a validating parser.

## What the transform repairs

Roslyn's grammar needs mutual (indirect) left recursion, which ANTLR rejects and
runtime 0.21.0 accepts via hub inlining (upstream #221). Beyond that, measured on
322 files of `dotnet/runtime`'s `System.Text.Json` (`src/`, `main` branch):

| | clean | notes |
|---|---|---|
| `grammars-v4` C# 7 (previous) | 93 | C# 8+ syntax unsupported |
| Roslyn, first working prep | 115 | interpolated strings failed |
| Roslyn, current prep | **317 / 322** | 5 with diagnostics, no crashes or timeouts |

Measured end to end through `mehen metrics`, not just the parser: ~179 s for the
corpus. All 5 remaining files are the directive-split-expression case below.

Note that a "clean" corpus count measures *parseability*, not correctness — this
grammar has now produced **twelve** distinct silent misparses: structurally wrong
trees with zero reported errors. Each was caught by a metric test or a parse-tree
dump, never by an error count.

| silent misparse | what the tree said instead |
|---|---|
| `declaration_expression` listed before `invocation_expression` | every method call was a declaration |
| bodiless members required a body | `void M();` fell through to `global_statement` |
| `parameter`'s elements all optional | `Zero()` had one empty parameter |
| `parameter`'s `type?` matches a tuple | `(a, b) => …` was a *simple* lambda |
| `SL_RAW_STRING_LIT` fenced with `""` | `var a = ""; f(); var b = "";` was ONE string token |
| `identifier_token` widened with `and`/`or`/`not` | `o is int and > 5` declared a variable named `and` |
| `constant_pattern` listed before `discard_pattern` | a `_ =>` arm is an expression, not the discard |
| `base_method_declaration` listed before the type forms | `record R(int X);` was a *method* named `R` |
| …and the same for `union` | `union U { }` was a *method* named `U` (with members it parsed correctly, hiding it) |
| `compilation_unit` did not end in `EOF` | `class C { } } } }` was a clean parse; the tail was never read |
| `incomplete_member` was reachable | `class C { int }` was a complete, error-free unit |
| `switch_statement`'s parens independently optional | `switch value { … }` parsed, though only the *expression* form is paren-free |
| a local generic declaration loses to the expression statement | `List<int> l;` reads as chained comparison — **open, issue #218** |

The first five *delete* code from the tree, the next five *relabel* it, and two accept
source that is not valid C# at all. Every shape is invisible to an error count, which is why the metric tests carry the load here
— see `crates/mehen-csharp/tests/lexer.rs`, whose assertions are all "did this
token span eat the statements after it".

Five of the twelve share one root cause: **an alternative that is viable for the
wrong input because a contextual keyword is a legal identifier.** Roslyn resolves
each semantically — it knows whether `F` names a type, whether `and` resolves to a
declared name, whether `record` is a keyword here — and a syntax-only grammar has
only alternative order and token identity to work with. Which of those two tools
applies is not a matter of taste; see the `record` section below for a case where
order alone provably cannot do it.

**No runtime capability is missing.** Every failure traced to either the prep or
an upstream-generator blind spot, and each was fixable declaratively — every one of
the 21 semantic coordinates lowers to a SemIR pattern via the derived
`patterns.toml`, with **no hooks at all** (the parser crate has no `src/hooks.rs`;
even the interpolated-string brace bookkeeping lives in the grammar's own lexer
actions). The gaps are catalogued below; `prepare-grammar.py` is the single source
of the transform, so each is reproducible rather than hand-patched.

Two of these were expensive to find, and both share a shape worth stating up
front: **the grammar generated cleanly and mis-parsed anyway.** Neither the
reserved-keyword nor the angle-bracket problem is visible in the grammar text —
they surface only by parsing real C# and comparing against the language.

### Dead-rule pruning: analysis upstream, deletion in the prep

Tokenizing Roslyn's lexical wrapper rules orphans 84 character-level helpers, and
they must be removed **before** literals are harvested — otherwise their
single-character literals become tokens that shadow `DEC_INT_LIT` and
`IDENTIFIER`, silently breaking every parse while generation stays clean. That
ordering is why the generator's own `--prune-unreachable` cannot do the job alone:
it runs inside codegen, after harvesting, so pruning there still emits the 78 junk
tokens (259 vs 181) and still mis-lexes.

The split is therefore **analysis upstream, edit locally**. The prep no longer
walks the grammar itself; it runs the generator as a reachability query and
deletes exactly the rules reported:

```text
antlr4-rust-gen <parser>.g4 --entry-rule compilation_unit
```

`G4S078` is already a dry run — it needs neither `--prune-unreachable` nor a
lexer nor an `--out-dir`, so the probe costs ~0.4 s and writes nothing. Iterated
to a fixpoint, because removing a rule can orphan helpers only it called.

This replaced a hand-rolled walker that scanned `\b[a-z_]\w*\b` over
comment-stripped text. Both produced byte-identical output on this grammar, but
the regex version could not distinguish a rule reference from a word inside an
action, a label, or an argument list — and it did produce a false positive
elsewhere (it wrongly called Kotlin's `script` unreachable). The generator walks
the real AST, so correctness now comes from one implementation rather than an
agreement between two.

### Reserved vs. contextual keywords

Roslyn spells every keyword as an inline literal, and the prep harvests those
literals into named tokens. That is correct for *reserved* keywords and wrong for
*contextual* ones: `var`, `record`, `from`, `get`, `and`, `required`, `_`, … are
ordinary identifiers wherever they have no special meaning, but a harvested token
wins the equal-length lexer match, so `var x = 1;` stopped parsing — `var` was
absent from the expected-token set entirely.

The remedy is the standard ANTLR one, and the same shape `grammars-v4`'s C#
grammar uses: widen `identifier_token` to accept all 42 contextual keywords back.

Two second-order lessons came out of this:

- **The blast radius is much wider than the keyword.** `var` appears in most
  idiomatic modern C#, so one wrong token classification presented as broken
  support for raw strings, ranges, `using` declarations, and unbound generics
  simultaneously. Seven probe failures had one cause.
- **`record_keyword` must *not* use the widened rule.** With
  `record_keyword : {pred}? identifier_token`, `partial struct S { }` predicts
  the record path (`record_keyword` = `partial`), and a predicate cannot prune a
  path ANTLR has already committed to — it surfaces as a hard error. It uses bare
  `IDENTIFIER` instead; `record` always lexes that way, so nothing is lost.

And a third, found later: **widening has a blast radius of its own.** Making
`and`/`or`/`not` legal identifiers is right in general and wrong in exactly one
position. `single_variable_designation : identifier_token` sits inside
`declaration_pattern : type variable_designation`, so

```csharp
o is int and > 5
```

binds `and` as a *variable named `and`, of type `int`* — and the `> 5` is orphaned
along with the combinator. `binary_pattern` is listed first among `pattern`'s
alternatives and still loses, because by the time the ATN reaches that choice the
designation alternative is already viable. The prep therefore narrows that one rule
to the contextual set *minus* the three combinators (`COMBINATOR_KEYWORDS`). They
stay legal names everywhere else, and `o is int and` — a designation genuinely
named `and` — is not valid C# anyway, since the compiler reads it as a combinator
too.

This is the mirror image of the `out _` gap below: there, a token that should have
been an identifier was not; here, tokens that should *not* be identifiers in one
position were. Both were invisible in the grammar text.

### `out _` and unbounded error recovery

`_` was initially excluded from the widening on the mistaken belief that it still
reached `IDENTIFIER`. It does not, so `F(out _)` could not parse — and the
consequences were far out of proportion to the gap. In `JsonObject.cs` a single
`out _` put the parser into error recovery, and recovery then accumulated
diagnostics without bound: **>4.29 × 10⁹** entries in the runtime's diagnostic
arena, **15.5 GB** peak RSS on a 406-line file, ending in either
`parser.rs:1847 diagnostic sequence arena fits in u32` or a stack overflow
depending on which resource ran out first. Fixing the one token removed all three
corpus crashes.

### Angle brackets vs. shift operators

The harvester minted `>>`, `>>>`, `>>=`, and `>>>=` as single tokens from
`binary_expression`, which made `List<List<int>>` unparsable — the final `>>`
lexed as one right-shift token that `type_argument_list`'s `'>'` can never match.
Roslyn has no such problem because its published grammar encodes no operator
precedence at all (that lives in the hand-written parser).

The prep emits only `'>'` and rebuilds the operators in the *parser* behind
`token_index_adjacent` adjacency predicates, exactly as the vendored C#7 grammar
does. `token_index_adjacent` compares only the last two consumed tokens, so a
three-piece operator carries the predicate at each junction.

The cost lands on the *consumer*: C# now spells three unrelated things with `<`/`>`
and `mehen-csharp`'s walker has to tell them apart from the enclosing rule alone,
because the token stream cannot.

| what it is | reaches the token scan as | how the walker knows |
|---|---|---|
| comparison `a < b` | `LT` / `GT` | default — counts |
| shift `a >> b` | two bare `GT` | `ChildHint::in_shift_operator` |
| generic `List<int>` | `LT` … `GT` | `ChildHint::in_type_delimiter` |

Both non-comparison cases were live bugs: a shift scored two ABC comparisons, and
every generic type scored two (`Dictionary<string, List<int>>` scored four). The
generic case is the more damaging of the two, since generics appear in essentially
every real C# file. `mehen-csharp/tests/abc.rs` pins each direction, including that
a comparison *beside* a generic type still counts and that a type argument may still
contain a real comparison.

### Optional type-body braces (performance)

Roslyn writes every type body as `'{'? member_declaration* '}'?` — both braces
independently optional — because its parser builds a complete declaration node
even for unterminated source. That is right for a node model and pathological for
a parsing grammar: after each member, prediction must weigh "another member"
against "the type ended without a `}`", recursively outward.

| members in one type | as-published | balanced pair |
|---|---|---|
| 32 | 2.28 s | 0.23 s |
| 64 | 6.55 s | 0.21 s |
| 128 | 22.54 s | **0.37 s** (61×) |

Rewriting to `('{' member_declaration* '}')?` keeps what the optionality is for
(a body-less `record R(int X);`) and drops only the half-present case; verified
behaviour-identical on the brace-less forms and on nested types.

### Smaller gaps

- **Accessor bodies.** `accessor_declaration` had no bare `;` alternative, so
  every auto-property (`{ get; set; }`) failed — 128 corpus files.
- **Parameter modifiers.** `ParameterSyntax.Modifiers` is an untyped
  `SyntaxList<SyntaxToken>` with no `<Kind>` children, so the generator emitted
  the declaration-modifier list, which lacks `out`, `in`, `params`, and `this`.
- **Auto-property initializers.** `Syntax.xml` wraps the property body in a
  `<Choice>` of `AccessorList` vs `(ExpressionBody | Initializer) Semicolon`.
  That drives the `SyntaxFactory` overload set, not the parser: `{ get; } = true;`
  is valid C# 6 and appears in 17 corpus files. The one case where the generator
  transcribes the model faithfully and the *model* is stricter than the language.
- **Binary integer literals.** Roslyn's `integer_literal_token` lists only
  decimal and hexadecimal; `0b1010` (C# 7.0) is absent.
- **Char-literal escapes.** A char literal holds exactly one character, so unlike
  a string it has no closure to absorb a mis-sized escape — `'\ud800'` needs the
  escape forms spelled out.
- **`incomplete_member` was reachable.** Roslyn's error-*recovery* node exists so the
  compiler can build a tree for source being typed, where `public int` is a member the
  author has not finished; Roslyn emits a diagnostic beside it, and the published
  grammar carries only the node. So `class C { int }` parsed as a complete, error-free
  compilation unit — which contradicts mehen's contract, where a clean parse is what
  tells `mehen metrics` to exit 0. Dropping the alternative makes it a syntax error and
  loses nothing legal: every real member form has its own rule.
- **The entry rule did not end in `EOF`.** Roslyn's parser reads a compilation unit
  and leaves the caller to check the stream position, so its grammar does not anchor
  `compilation_unit`. A syntax-only parser therefore stopped at the first token it
  could not continue with and reported success on the prefix: `class C { } } } }`
  was a clean parse with the stray braces never read. Anchoring makes the unconsumed
  tail a syntax error. It has to run **after** pruning — the generator treats every
  rule reaching `EOF` as an entry point, so anchoring first makes nothing
  unreachable and the 84 orphaned helpers survive.
- **Verbatim interpolated strings.** `$@"…"` needs its own lexer mode: a
  backslash is literal there and `""` is the escaped quote, so one text rule
  cannot serve both flavours. Both prefix orders are legal (`$@"` and `@$"`) and
  Roslyn spells only the first, so the second needs an explicit alternative.
- **Raw string fences are three quotes, in *both* forms.** The single- vs
  multi-line distinction is whether the content holds a newline, not a shorter
  fence — a `""` fence collides with the empty string literal and eats code (see
  the silent-misparse table above). Rule order carries what a context-free rule
  cannot express: the single-line form first, since both match a one-liner over the
  same extent, while `~[\r\n]` structurally keeps it from claiming a multi-line one.
- **Interpolated raw strings** (`$"""a{x}b"""`) need a *third* text mode. Roslyn
  spells the opening fence as three parser tokens (`DOLLAR+ TRIPLE_DQUOTE DQUOTE*`),
  but the text between holes cannot be lexed in the default mode — the `a` comes
  back as an `IDENTIFIER` — so both Roslyn start-token rules are retargeted at one
  mode-pushing token. Quotes are literal content inside; only a run of three closes
  the string.
- **Token names must not be index-derived.** The prep rejects the generator's
  `OP_nnn` fallback because a literal's position shifts when any other literal is
  added or removed, silently rebinding tokens that hand-written code names. The
  same reasoning applies to *collision* suffixes: `U8` and `u8` both want `KW_U8`,
  and disambiguating by index reintroduces exactly the instability the check exists
  to prevent. The suffix is derived from the literal's own spelling instead
  (`KW_U8` / `KW_U8_LOWER`).

### Known remaining limitation: directive-split expressions

All 5 files still reporting errors are the same class: a preprocessor directive
splitting a single expression.

The fifth (`JsonDocument.Parse.cs`) only *became* visible when `incomplete_member`
was dropped: the directive splits a method's return type across `#if` branches, so
the parser sees two types where one belongs, and the first of them used to match as
an incomplete member. It was always this same limitation — the recovery node was
hiding it behind a clean parse.

```csharp
if (
#if NET9_0
    !dict.TryAdd(propertyName, value)
#else
    !dict.TryAdd(propertyName, value, out int index)
#endif
    )
```

mehen deliberately routes directives to a channel without evaluating them, so
both branches reach the parser and cannot both be one expression. That is the
intended trade: evaluating `#if` would mean picking a symbol set, and metrics for
a subset of the code is worse than approximate metrics for all of it.

### Roslyn's "omitted" syntax nodes

Two rules are genuinely empty productions modelling a blank slot:
`omitted_type_argument` (the unbound generic `Dictionary<,>`) and
`omitted_array_size_expression` (the multi-dimensional `int[,]`). ANTLR forbids
an empty rule inside a closure, so the rules must go — but simply deleting their
alternatives **loses real syntax**: the `','` in
`'[' (expression (',' expression)*)? ']'` then has nothing to match on either
side, and `int[,]` / `Dictionary<,>` stop parsing. The prep instead makes the
list elements optional at the two use sites, which is exactly what an empty node
expressed there. `repro/roslyn-csharp-perf/fixtures/omitted-nodes.cs` is the
regression test; `run.sh` asserts it parses with zero errors.

### The `record` contextual keyword

Roslyn's `Syntax.xml` declares the record keyword as
`<ContextualKind Name="RecordKeyword"/>`, and its grammar generator reads only
`<Kind>` children of a `<Field>`. That is the **only** `<ContextualKind>` in the
whole file (versus 1018 plain `<Kind>`), so it is the single field that hits the
blind spot: the published grammar has **no `'record'` literal at all** and falls
back to the catch-all `syntax_token`, which accepts every identifier, keyword,
literal, operator, and punctuation token.

The cost was severe — `class` became viable as both `class_declaration` and
`record_declaration`, so full-context prediction carried the impossible record
path across every member boundary:

| members in one class | as-published | record restored |
|---|---|---|
| 4 | 188 ms | 27 ms |
| 12 | 2 166 ms | 204 ms |
| 24 | 12 160 ms | **423 ms** |

One real 953-line file (`JsonDocument.Parse.cs`) took **272 s**; the whole
library timed out past 600 s. Restoring the keyword brought that to
~3 m 50 s, and the balanced-brace fix above took it the rest of the way to the
~208 s / 5.1 s-worst-file figures at the top of this section.

The prep restores it as a *contextual* keyword: the prep mints a real `KW_RECORD`
token so the lexer distinguishes the word, and then widens `identifier_token` with
it so `record` stays legal as an ordinary name (`int record = 1;`).

Getting there took three attempts, and the two that failed are instructive because
each looked sufficient:

1. **`record_keyword : {IsRecordKeyword()}? IDENTIFIER`** — a predicate on the token
   text, lowered to a pure SemIR comparison. Fixes the performance collapse above and
   is what shipped first. But `member_declaration`'s alternatives are alphabetical, so
   `base_method_declaration` precedes the type forms, and `record` is a legal `type` —
   so `record R(int X);` matched `method_declaration` with `record` as the return type
   and `R` as the method name. Every positional record was a phantom method.
2. **Predicate + hoist `record_declaration` first.** Fixes records; breaks 29 corpus
   files. Hoisting puts the record path on the *committed* path for an ordinary
   property, so `T P { get => 1; set { } }` predicts `record_keyword` = `T` — and **a
   predicate cannot prune a path ANTLR has already committed to**, so it surfaces as a
   hard error rather than a silent rejection. This is the same wall the note in
   `RECORD_KEYWORD_RULE` describes for `partial struct S { }`.
3. **A real token + the hoist.** Both halves are required and neither suffices:
   without the token the hoist breaks properties; without the hoist `record` is still a
   viable `type` (it has to be, to stay a legal name) and the phantom method returns.
   With a real token, `T P { … }` cannot predict the record path at all, so the hoist
   is safe.

The residual trade is a method whose return type is a class *literally named*
`record`, which now reads as a record declaration. That is the only shape affected,
and `record` as a type name is vanishingly rare in real C#.

The performance half was diagnosed by the antlr-rust-runtime team on
[`antlr-rust-runtime#248`](https://github.com/ophi-dev/antlr-rust-runtime/issues/248);
`repro/roslyn-csharp-perf/` at the repo root reproduces both variants.

## Semantic helpers — no hooks anywhere

The vendored grammar is **unmodified** — every repair above is applied by
`prepare-grammar.py` on the way to the derived pair, never by editing
`CSharp.Generated.g4`.

Roslyn's grammar declares no `superClass` and calls no host-language helpers (it
is generated from a syntax model, not hand-written for a parser generator), so
unlike the `grammars-v4` grammar there is no base class to port. The one semantic
surface the derived *parser* uses is introduced by the transform and lowers to
**pure SemIR patterns** in the derived `patterns.toml` — no parser hook object
exists:

- `IsRightShift` and friends — the angle-bracket adjacency checks
  (`token_index_adjacent`).

(The `record` contextual keyword was a second one, `IsRecordKeyword`, until the
predicate proved unable to carry it; see the section above. It is now a real
`KW_RECORD` token, so the helper is gone.)

The **lexer** is hand-written, because Roslyn publishes none — but its state
lives in the grammar too, in `@lexer::members`, and lowers through the same
`patterns.toml`. A `}` cannot know from the grammar alone whether it closes an
interpolation hole or a nested block:

```csharp
$"a{ new[]{ 1, 2 }.Length }b"
//        ^^^^^^^^^  must NOT end the hole
```

Telling them apart needs a brace depth per open hole and a *conditional* mode pop
— and SemIR has no conditional and no mode-changing action (its seven statements
all touch member state). The grammar therefore splits the `}` into two
predicate-gated rules over the same character, each carrying its own
*unconditional* command, so **rule selection** supplies the condition:

```antlr
INTERP_NESTED_CLOSE : {nestDepth > 0}?       '}' { nestDepth--; }
INTERP_HOLE_CLOSE   : {holeStack.Count > 0}? '}' -> popMode
```

Order is load-bearing in two ways. Between those rules, the lowering DSL has only
`not` and truthiness — no comparisons, no `&&` — so the deeper case must come
first: reaching the second rule already proves `nestDepth == 0`. And both must
precede the unguarded `RBRACE` fallback, which is why `prepare-grammar.py` keeps
`{`, `}` and `:` out of the harvested literals block (`HOLE_SENSITIVE_LITERALS`)
and lets the hand-written file define them.

Stack-valued member state is runtime 0.20.1+, from
[`antlr-rust-runtime#206`](https://github.com/ophi-dev/antlr-rust-runtime/issues/206) —
filed for exactly this grammar shape.

Generation runs with `--sem-unknown error --require-full-semantics`, so any *new*
helper appearing in a future grammar update fails `cargo xtask antlr generate
csharp` instead of silently degrading parse fidelity. `tests/hooks.rs` pins the
observable consequences — nested braces in holes, format clauses, verbatim
backslashes, and nested interpolated strings restoring the enclosing depth.

## Toolchain

| Tool | Version | Why |
|---|---|---|
| Rust runtime + generator | [`ophi-dev/antlr-rust-runtime`](https://github.com/ophi-dev/antlr-rust-runtime) `v0.25.0` | Codegen; also computes rule reachability for the transform |
| [`uv`](https://docs.astral.sh/uv/) | any recent | Runs `prepare-grammar.py`; the script's PEP 723 block pins the interpreter |

Unlike the other ANTLR targets, C# needs `uv`: its grammar is derived rather than
directly generatable. `xtask` probes for it and only errors on the C# target, so a
missing `uv` does not block Kotlin/Java regeneration.

Regenerate with:

```bash
cargo install antlr-rust-runtime --version 0.25.0 --features codegen --bin antlr4-rust-gen --force
cargo run -p xtask -- antlr generate csharp
```
