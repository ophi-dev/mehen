// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! ABC (Assignments / Branches / Conditions) tests for the ANTLR C# walker.
//!
//! - **A**: `=` and every compound form (including `??=`), `++`/`--`, and any
//!   initialized declarator.
//! - **B**: function/method calls and object creation. A *qualified* call
//!   (`o.M()`) is ONE branch — the `.M` qualification is not itself a call.
//! - **C**: `if`/`case`/`catch`/`when`, loops, comparisons, equality,
//!   `&&`/`||`, the ternary, `??`, `is`, and `as`. Bit-shifts are excluded.

mod common;

use common::analyze_clean;

/// The `(assignments, branches, conditions)` triple for the whole unit.
fn abc(source: &str) -> (u64, u64, u64) {
    let a = analyze_clean(source);
    let m = mehen_report::metrics_json::abc(&a.root.metrics);
    (m.assignments as u64, m.branches as u64, m.conditions as u64)
}

#[test]
fn plain_assignment_and_initialized_local() {
    // `int x = 1;` (declarator init) + `x = 2;` (assignment) = 2 A.
    assert_eq!(abc("class C { void F() { int x = 1; x = 2; } }"), (2, 0, 0));
}

#[test]
fn compound_and_null_coalescing_assignment_count() {
    // `+=` and `??=` are both assignments.
    let (a, _, _) = abc("class C {
             void F(int i, string s) {
                 i += 1;
                 s ??= \"d\";
             }
         }");
    assert_eq!(a, 2);
}

#[test]
fn increment_and_decrement_are_assignments() {
    let (a, _, _) = abc("class C { void F(int i) { i++; i--; } }");
    assert_eq!(a, 2);
}

#[test]
fn uninitialized_declaration_is_not_an_assignment() {
    assert_eq!(abc("class C { void F() { int x; } }"), (0, 0, 0));
}

#[test]
fn a_qualified_call_is_exactly_one_branch() {
    // `o.Helper()` is ONE branch: the `.Helper` member access is qualification,
    // not a second call. This pins the fix for an early 3x over-count.
    let (_, b, _) = abc("class C {
             void Helper() { }
             void F(C o) { o.Helper(); }
         }");
    assert_eq!(b, 1);
}

#[test]
fn a_field_read_is_not_a_branch() {
    let (_, b, _) = abc("class C {
             public int Field;
             int F(C o) { return o.Field; }
         }");
    assert_eq!(b, 0);
}

#[test]
fn deeply_qualified_call_is_still_one_branch() {
    let (_, b, _) = abc("class C { void F(int x) { System.Console.WriteLine(x); } }");
    assert_eq!(b, 1);
}

#[test]
fn object_creation_is_a_branch() {
    let (_, b, _) = abc("class C { object F() { return new object(); } }");
    assert_eq!(b, 1);
}

#[test]
fn constructor_initializer_is_a_branch() {
    // `: this(0)` chains to another constructor — a call.
    let (_, b, _) = abc("class C {
             public C() : this(0) { }
             public C(int x) { }
         }");
    assert_eq!(b, 1);
}

#[test]
fn if_plus_its_comparison_are_two_conditions() {
    // The `if` itself is one condition and the `<` comparison another —
    // matching `mehen-java` on the same shape.
    let (_, _, c) = abc("class C { int F(int v) { if (v < 0) { return 1; } return 2; } }");
    assert_eq!(c, 2);
}

#[test]
fn equality_and_relational_operators_are_conditions() {
    let (_, _, c) = abc("class C {
             bool F(int a, int b) {
                 return a == b || a != b || a < b || a > b || a <= b || a >= b;
             }
         }");
    // 6 comparisons + 5 `||` operators = 11.
    assert_eq!(c, 11);
}

#[test]
fn bit_shifts_are_not_conditions() {
    // `<<`/`>>` share the bare `LT`/`GT` tokens with comparisons in this
    // grammar, so this pins that the walker tells them apart.
    let (_, _, c) = abc("class C { int F(int v) { return (v << 2) >> 1; } }");
    assert_eq!(c, 0);
}

#[test]
fn is_and_as_type_tests_are_conditions() {
    let (_, _, c) = abc("class C {
             bool F(object o) { return o is string; }
             string G(object o) { return o as string; }
         }");
    assert_eq!(c, 2);
}

#[test]
fn null_coalescing_is_a_condition() {
    let (_, _, c) = abc("class C { string F(string s) { return s ?? \"d\"; } }");
    assert_eq!(c, 1);
}

#[test]
fn catch_and_when_filter_are_conditions() {
    // `catch` is one condition; the `when` filter adds another; the filter's
    // own `>` comparison adds a third.
    let (_, _, c) = abc("class C {
             void F(int code) {
                 try { }
                 catch (System.Exception) when (code > 0) { }
             }
         }");
    assert_eq!(c, 3);
}

#[test]
fn case_labels_are_conditions_but_default_is_not() {
    // 2 `case` labels = 2 conditions; `default:` adds none.
    let (_, _, c) = abc("class C {
             int F(int v) {
                 switch (v) {
                     case 1: return 1;
                     case 2: return 2;
                     default: return 0;
                 }
             }
         }");
    assert_eq!(c, 2);
}

#[test]
fn attribute_arguments_record_no_executable_complexity() {
    // An attribute is compile-time metadata: its `= …` named argument must not
    // count as an assignment, nor its operators as conditions.
    assert_eq!(
        abc("class C {
                 [System.Obsolete(\"x\", true)]
                 void F() { }
             }"),
        (0, 0, 0)
    );
}

#[test]
fn generic_delimiters_are_not_conditions() {
    // REGRESSION. C# spells generic argument lists with the same `<`/`>` tokens as
    // a comparison, so `List<int>` scored two ABC conditions and
    // `Dictionary<string, List<int>>` scored four — inflating the score of any file
    // that mentions a generic type, which in practice is all of them.
    let (_, _, c) = abc("class C {
             System.Collections.Generic.Dictionary<string,
                 System.Collections.Generic.List<int>> Map;
         }");
    assert_eq!(c, 0);
}

#[test]
fn a_generic_type_does_not_mask_a_real_comparison_beside_it() {
    // The delimiter exclusion must be positional, not a blanket `<`/`>` mute: a
    // comparison in the same expression as a generic type still counts.
    let (_, _, c) = abc("class C {
             bool F(System.Collections.Generic.List<int> items, int limit) {
                 return items.Count < limit;
             }
         }");
    assert_eq!(c, 1);
}

#[test]
fn a_generic_type_argument_may_still_contain_a_comparison() {
    // The hint deliberately does not propagate into children, so a comparison
    // *inside* a type argument's lambda still counts.
    let (_, _, c) = abc("class C {
             System.Func<int, bool> F() {
                 return x => x > 0;
             }
         }");
    assert_eq!(c, 1);
}

#[test]
fn every_shift_assignment_form_counts_one_assignment() {
    // REGRESSION. `>>=` and `>>>=` are the only assignment operators the prep
    // splits into separate tokens (`GT GE` / `GT GT GE`), so they arrive as child
    // *rules* rather than as an operator terminal. `a >>= 2` scored no assignment
    // at all while the otherwise-identical `a <<= 2` scored one.
    for op in ["<<=", ">>=", ">>>=", "+=", "&="] {
        let (a, _, _) = abc(&format!("class C {{ void F(int v) {{ v {op} 2; }} }}"));
        assert_eq!(a, 1, "`{op}` must count exactly one assignment");
    }
}

#[test]
fn nameof_is_not_a_branch() {
    // REGRESSION. `nameof` is a contextual keyword with no grammar rule of its
    // own, so `nameof(x)` has the invocation shape — but it is a compile-time
    // operator that yields a string constant, calls nothing, and never evaluates
    // its argument. Counting it ranked
    // `throw new ArgumentNullException(nameof(arg))` above the same throw with a
    // literal.
    let (_, b, _) = abc("class C { string F() { return nameof(F); } }");
    assert_eq!(b, 0);
}

#[test]
fn a_real_call_is_still_a_branch_beside_nameof() {
    // The `nameof` exclusion is by callee, so an ordinary call in the same method
    // still counts — including a *method* named `nameof`, which is legal.
    let (_, b, _) = abc("class C {
             void G() { }
             string F() { G(); return nameof(F); }
         }");
    assert_eq!(b, 1);
}

#[test]
fn pattern_combinators_are_conditions() {
    // REGRESSION, twice over. C# 9 spells pattern combinators with the contextual
    // keywords `and`/`or`/`not` rather than the `&&`/`||`/`!` operator tokens the
    // token scan sees, so a pattern-heavy method scored as straight-line code.
    //
    // The grammar had to be fixed too: widening `identifier_token` with the
    // contextual keywords made `o is int and > 5` bind `and` as a *variable name*
    // of type `int` via `declaration_pattern`, silently dropping the combinator and
    // the `> 5` with it — with zero diagnostics.
    let (_, _, c) = abc("class C { bool F(object o) { return o is int and long; } }");
    assert_eq!(c, 2, "the `is` test plus the `and` combinator");
}

#[test]
fn a_relational_pattern_counts_its_comparison_once() {
    // `is > 5` is the `is` test plus one comparison — the pattern rule must not
    // record a second condition on top of the operator token.
    let (_, _, c) = abc("class C { bool F(int v) { return v is > 5; } }");
    assert_eq!(c, 2);
}

#[test]
fn a_combinator_keyword_is_still_a_legal_name_elsewhere() {
    // Excluding `and`/`or`/`not` from `single_variable_designation` must not make
    // them reserved: each is still a valid field, parameter, and local name.
    // One assignment — the `int not = or;` initializer; the uninitialized field
    // declarator is not one.
    assert_eq!(
        abc("class C {
                 int and;
                 int F(int or) { int not = or; return and + not; }
             }"),
        (1, 0, 0)
    );
}

#[test]
fn switch_expression_arms_are_conditions_but_the_discard_is_not() {
    // REGRESSION. A switch *expression* scored nothing: no decision per arm and no
    // cognitive nesting, so rewriting a switch statement into the expression form
    // silently lowered the score. The discard arm (`_ =>`) is the fall-through
    // rather than a test, so it counts no more than `default:` does.
    let (_, _, c) = abc("class C {
             int F(int v) {
                 return v switch { 1 => 1, 2 => 2, _ => 0 };
             }
         }");
    assert_eq!(c, 2);
}

#[test]
fn a_split_shift_operator_is_one_halstead_operator() {
    // `>>` is spelled as two adjacent `>` tokens so a generic closer is never
    // mis-lexed as a shift (see the parser crate's PROVENANCE). Recording each
    // `>` would inflate Halstead length and conflate the shift with the `>`
    // comparison in the distinct-operator set, so the shift is recorded once at
    // its enclosing rule. Pinned against an equivalent single-token operator.
    let length = |source: &str| {
        let a = analyze_clean(source);
        mehen_report::metrics_json::halstead(&a.root.metrics).length
    };
    assert_eq!(
        length("class C { int M(int a) => a >> 1; }"),
        length("class C { int M(int a) => a * 1; }"),
        "a shift must cost the same Halstead length as any other binary operator"
    );
}

#[test]
fn an_operator_declarations_symbol_is_not_an_assignment() {
    // REGRESSION. Roslyn spells an operator's symbol as a direct token choice on the
    // declaration (`… KW_OPERATOR KW_CHECKED? (PLUS | PLUS_PLUS | …)`), so the `++` in
    // `operator ++(C v)` reached the token scan looking exactly like a real increment
    // and scored an ABC assignment — for the *declaration* of an operator, which
    // mutates nothing.
    let (a, _, _) = abc("class C { public static C operator ++(C v) => v; }");
    assert_eq!(a, 0);
}

#[test]
fn an_operator_declarations_symbol_is_not_a_condition() {
    // The same suppression covers the comparison and boolean operators: declaring
    // `operator <` must not score a comparison from its own signature.
    let (_, _, c) = abc("class C {
             public static bool operator <(C a, C b) => true;
             public static bool operator >(C a, C b) => true;
         }");
    assert_eq!(c, 0);
}

#[test]
fn an_operator_body_still_counts_its_operators() {
    // The suppression is positional, not a blanket mute on operator declarations: a
    // real `++` inside the body counts, alongside the initializer's `=`.
    let (a, _, _) = abc("class C {
             public static C operator +(C x, C y) {
                 int i = 0;
                 i++;
                 return x;
             }
         }");
    assert_eq!(a, 2, "the `int i = 0` initializer plus the `i++`");
}

#[test]
fn a_utf8_literal_suffix_is_part_of_the_operand() {
    // REGRESSION. `"text"u8` is one literal in real C#; Roslyn's grammar spells the
    // suffix as a separate trailing token only because it models the syntax node that
    // way. Classifying it as a Halstead *operator* made a UTF-8 literal cost a
    // distinct operator that no operator was applied in — pinned against the plain
    // literal, which must have the same operator vocabulary.
    let halstead = |source: &str| {
        let a = analyze_clean(source);
        mehen_report::metrics_json::halstead(&a.root.metrics)
    };
    let utf8 = halstead("class C { System.ReadOnlySpan<byte> F() => \"text\"u8; }");
    let plain = halstead("class C { System.ReadOnlySpan<byte> F() => \"text\"; }");
    assert_eq!(
        utf8.n1, plain.n1,
        "the `u8` suffix must not add a distinct operator"
    );
    assert_eq!(utf8.big_n1, plain.big_n1, "nor an operator occurrence");
}

#[test]
fn an_auto_property_initializer_is_an_assignment() {
    // REGRESSION. `public int P { get; set; } = 5;` carries its `equals_value_clause`
    // directly on `property_declaration` rather than through a `variable_declarator`,
    // so it scored no assignment while the equivalent field `public int P = 5;`
    // scored one.
    let (a, _, _) = abc("class C { public int P { get; set; } = 5; }");
    assert_eq!(a, 1);
    // And an uninitialized auto-property is still not an assignment.
    let (a, _, _) = abc("class C { public int P { get; set; } }");
    assert_eq!(a, 0);
}

#[test]
fn a_query_let_binding_is_an_assignment() {
    // REGRESSION. `let_clause : KW_LET identifier_token EQ expression` puts its `=` as
    // a bare token on a rule that is not part of the inlined `expression`, so neither
    // the token scan nor the expression classifier saw it — a `let` bound a name with
    // no assignment recorded.
    let (a, _, _) = abc("class C {
             static object F(int[] s) { return from x in s let y = x select y; }
         }");
    assert_eq!(a, 1);
}

#[test]
fn a_user_symbol_named_nameof_is_still_a_branch() {
    // REGRESSION. `nameof` is only *contextual*, so a delegate can legally be named
    // `nameof` — and the operator takes exactly one argument, so a two-argument call
    // cannot be it. A text-only callee check suppressed the real delegate call.
    let (_, b, _) = abc("class C {
             static int F() {
                 System.Func<int, int, int> nameof = (x, y) => x + y;
                 return nameof(1, 2);
             }
         }");
    assert_eq!(b, 1, "the delegate call is a real branch");
}

#[test]
fn the_nameof_operator_is_still_suppressed() {
    // The counterpart: the arity guard must not stop suppressing the actual operator,
    // which is always one argument.
    let (_, b, _) = abc("class C { static string F() => nameof(F); }");
    assert_eq!(b, 0);
}

#[test]
fn a_utf8_literal_costs_the_same_as_a_plain_one() {
    // REGRESSION, twice. `"text"u8` is ONE literal in C#; Roslyn splits the suffix off
    // only to model the syntax node, and the preceding `STRING_LIT` has already
    // recorded the operand. Classifying the suffix as an operator was wrong (nothing is
    // applied), and classifying it as an *operand* was also wrong — that gave one
    // literal two operand occurrences. It contributes nothing.
    let halstead = |source: &str| {
        let a = analyze_clean(source);
        mehen_report::metrics_json::halstead(&a.root.metrics)
    };
    let utf8 = halstead("class C { static System.ReadOnlySpan<byte> F() => \"text\"u8; }");
    let plain = halstead("class C { static System.ReadOnlySpan<byte> F() => \"text\"; }");
    assert_eq!(utf8.n1, plain.n1, "no extra distinct operator");
    assert_eq!(utf8.n2, plain.n2, "no extra distinct operand");
    assert_eq!(utf8.length, plain.length, "no extra length");
}

#[test]
fn the_contextual_field_keyword_is_an_operand() {
    // REGRESSION. C# 14's semi-auto property (`get => field;`) references the
    // compiler-synthesized backing field. In expression position that is a value
    // reference like `this` or `base` — Roslyn even gives it its own
    // `field_expression : KW_FIELD` rule — but the token does not pass through
    // `identifier_token`, so it fell through as a Halstead *operator*: a spurious
    // operator plus a missing operand.
    let halstead = |source: &str| {
        let a = analyze_clean(source);
        mehen_report::metrics_json::halstead(&a.root.metrics)
    };
    let semi_auto = halstead("class C { public int P { get => field; } }");
    let explicit = halstead("class C { int _x; public int P { get => _x; } }");
    assert_eq!(
        semi_auto.n1, explicit.n1,
        "`field` must not add a distinct operator"
    );
}

#[test]
fn stack_allocation_is_a_branch() {
    // REGRESSION. `stackalloc` allocates exactly as `new` does, just on the stack, but
    // `stack_alloc_array_creation_expression` and its implicit form were missing from
    // the creation list — so `stackalloc int[4]` scored no branch while `new int[4]`
    // scored one.
    let (_, b, _) = abc("class C { static void F() { var s = stackalloc int[4]; } }");
    assert_eq!(b, 1);
    let (_, b, _) = abc("class C { static void F() { var s = stackalloc[] { 1, 2 }; } }");
    assert_eq!(b, 1);
}

#[test]
fn a_primary_constructors_base_call_is_a_branch() {
    // REGRESSION. `class D(int x) : B(x)` is the primary-constructor spelling of a
    // base-constructor call, but it reaches `primary_constructor_base_type` rather than
    // `constructor_initializer` — so it scored 0 branches where the explicit
    // `D(int x) : base(x)` scored 1. Pinned against that form.
    let primary = abc("class B { public B(int x) { } }
         class D(int x) : B(x) { }");
    let explicit = abc("class B { public B(int x) { } }
         class D : B { public D(int x) : base(x) { } }");
    assert_eq!(primary.1, 1);
    assert_eq!(primary.1, explicit.1);
}

#[test]
fn a_linq_where_clause_is_a_condition() {
    // REGRESSION. A `where` is the query-expression equivalent of an `if` — a filter
    // predicate — and was recording nothing. A predicate that is already boolean has no
    // comparison for the token scan to catch, so `where enabled` scored 0.
    let (_, _, c) = abc("class C {
             static object F(int[] xs, bool enabled) { return from x in xs where enabled select x; }
         }");
    assert_eq!(c, 1);
    // With a comparison it is two, exactly as `if (x > 0)` is two.
    let (_, _, c) = abc("class C {
             static object F(int[] xs) { return from x in xs where x > 0 select x; }
         }");
    assert_eq!(c, 2);
}

#[test]
fn a_bare_default_literal_is_an_operand() {
    // REGRESSION. Roslyn groups bare `default` under `literal_expression` beside
    // `true`/`false`/`null`, which are operands — but `KW_DEFAULT` does not pass
    // through `identifier_token`, so it fell through as a Halstead *operator*: a
    // spurious operator plus a missing value operand.
    let halstead = |source: &str| {
        let a = analyze_clean(source);
        mehen_report::metrics_json::halstead(&a.root.metrics)
    };
    let default_lit = halstead("class C { static void F() { string v = default; } }");
    let null_lit = halstead("class C { static void F() { string v = null; } }");
    assert_eq!(
        default_lit.n1, null_lit.n1,
        "`default` must cost the same as `null`"
    );
}

#[test]
fn anonymous_object_creation_is_a_branch() {
    // REGRESSION. `new { A = 1 }` has no `argument_list`, so `classify_expression`'s
    // invocation shape never saw it, and `anonymous_object_creation_expression` was
    // missing from the creation list — a real allocation scored nothing while
    // `new object()` scored one.
    let (_, b, _) = abc("class C { static object F() => new { A = 1 }; }");
    assert_eq!(b, 1);
}

#[test]
fn a_collection_expression_is_a_branch() {
    // REGRESSION. C# 12's `int[] v = [1, 2];` allocates exactly as `new[] { 1, 2 }`
    // does — the spelling changed, not the operation — but `collection_expression` was
    // missing from the creation list, so it scored 0 where the older form scored 1.
    let collection = abc("class C { static int[] F() { int[] v = [1, 2]; return v; } }");
    let explicit = abc("class C { static int[] F() { int[] v = new[] { 1, 2 }; return v; } }");
    assert_eq!(collection.1, 1);
    assert_eq!(collection.1, explicit.1);
}

#[test]
fn a_named_anonymous_object_member_is_an_assignment() {
    // REGRESSION. Roslyn puts the `A =` of `new { A = 1 }` in a `name_equals` child of
    // `anonymous_object_member_declarator`, so it is neither an assignment-shaped
    // `expression` nor an `equals_value_clause` — it recorded nothing, while the
    // equivalent `new C { A = 1 }` recorded one.
    let (a, _, _) = abc("class C { static object F() => new { A = 1 }; }");
    assert_eq!(a, 1);
}

#[test]
fn an_inferred_anonymous_member_is_not_an_assignment() {
    // The counterpart: `new { x }` infers the member name from the expression and
    // assigns nothing explicitly, so it has no `name_equals` child and records nothing.
    let (a, _, _) = abc("class C { static object F(int x) => new { x }; }");
    assert_eq!(a, 0);
}

#[test]
fn a_using_alias_is_not_an_assignment() {
    // `name_equals` is shared with using-alias and attribute-argument names, which is
    // why the match is at the *declarator* rather than at `name_equals` itself.
    assert_eq!(abc("using S = System.String;\nclass C { }"), (0, 0, 0));
}

#[test]
fn an_empty_interpolated_string_is_one_operand() {
    // REGRESSION. `$""` produces no `INTERPOLATED_TEXT` token at all — only the start
    // and end delimiters, which are operators — so it contributed zero Halstead
    // operands where `""` contributes one, skewing volume and the maintainability
    // index. Mirrors `mehen-kotlin`'s `classify_empty_string_operand`.
    let halstead = |source: &str| {
        let a = analyze_clean(source);
        mehen_report::metrics_json::halstead(&a.root.metrics)
    };
    let interpolated = halstead("class C { static string F() { var s = $\"\"; return s; } }");
    let plain = halstead("class C { static string F() { var s = \"\"; return s; } }");
    assert_eq!(interpolated.n2, plain.n2, "empty `$\"\"` is one operand");
}

#[test]
fn a_generic_lists_comma_is_still_an_operator() {
    // REGRESSION introduced by the `in_type_delimiter` fix itself: the hint marks a
    // whole delimiter *list*, and the Halstead branch returned for every token in it —
    // dropping the `,` between type arguments along with the `>`. Only `<`/`>` are the
    // delimiter; a comma is ordinary punctuation and counts as it does in a parameter
    // list.
    let halstead = |source: &str| {
        let a = analyze_clean(source);
        mehen_report::metrics_json::halstead(&a.root.metrics)
    };
    let two = halstead("class C { System.Collections.Generic.Dictionary<int, string> F; }");
    let one = halstead("class C { System.Collections.Generic.List<int> F; }");
    // Two extra distinct operators over the one-argument form: the `,` and the extra
    // type name's operand is separate — so the delta must be 2, not 1.
    assert_eq!(
        two.n1 - one.n1,
        2.0,
        "the type-argument comma must count as an operator"
    );
}

#[test]
fn overloaded_true_and_false_symbols_are_operators() {
    // REGRESSION. `operator true` / `operator false` are the only overloadable
    // operators whose symbols are keywords that mean something else elsewhere — as a
    // *literal*, `true` is a Halstead operand, and the declaration reused the same
    // token. So declaring them added operands rather than operators. `in_operator_symbol`
    // already marked the position; it just did not reach the Halstead classification.
    let halstead = |source: &str| {
        let a = analyze_clean(source);
        mehen_report::metrics_json::halstead(&a.root.metrics)
    };
    let bool_ops = halstead(
        "class C {
             public static bool operator true(C c) => true;
             public static bool operator false(C c) => false;
         }",
    );
    // Two distinct operators for the two declared symbols. The `true`/`false` in the
    // bodies are still operands, so n2 is unaffected.
    let plain = halstead("class C { public static C operator +(C a, C b) => a; }");
    assert!(
        bool_ops.n1 > plain.n1,
        "declaring `operator true`/`false` must add distinct operators, \
         got n1 {} vs {}",
        bool_ops.n1,
        plain.n1
    );
}

#[test]
fn a_brace_only_array_initializer_is_a_branch() {
    // REGRESSION. `int[] v = { 1, 2 };` has no `new` and no `[…]`, so Roslyn puts a bare
    // `initializer_expression` on the right-hand side and nothing in the creation list
    // fired — it scored 0 where `new[] { 1, 2 }` and `[1, 2]` each scored 1, making ABC
    // depend on which of three equivalent spellings the author used.
    let bare = abc("class C { static int[] F() { int[] v = { 1, 2 }; return v; } }");
    let explicit = abc("class C { static int[] F() { int[] v = new[] { 1, 2 }; return v; } }");
    let collection = abc("class C { static int[] F() { int[] v = [1, 2]; return v; } }");
    assert_eq!(bare.1, 1);
    assert_eq!(bare.1, explicit.1);
    assert_eq!(bare.1, collection.1);
}

#[test]
fn a_creations_own_initializer_is_not_a_second_branch() {
    // The guard: a creation *nests* an initializer for its elements, so counting the rule
    // unconditionally would score `new[] { 1, 2 }` twice. Nested creations too —
    // `new[] { new[] { 1 } }` is two allocations, not four.
    let nested = abc("class C {
             static int[][] F() { int[][] v = new[] { new[] { 1 } }; return v; }
         }");
    assert_eq!(nested.1, 2, "two creations, each counted once");
}

#[test]
fn a_nested_bare_initializer_is_one_allocation() {
    // REGRESSION introduced by the bare-initializer fix: a rectangular array
    // `int[,] v = { { 1, 2 }, { 3, 4 } };` has three `initializer_expression` nodes and
    // scored three branches, where the explicit `new int[,] { … }` scored one — the
    // creation set the hint before its initializers were reached, but a *bare* outer
    // initializer did not mark its own nested groups.
    let bare =
        abc("class C { static int[,] F() { int[,] v = { { 1, 2 }, { 3, 4 } }; return v; } }");
    let explicit = abc("class C {
             static int[,] F() { int[,] v = new int[,] { { 1, 2 }, { 3, 4 } }; return v; }
         }");
    assert_eq!(bare.1, 1, "one array allocated");
    assert_eq!(bare.1, explicit.1);
}

#[test]
fn an_identifiers_operand_is_its_name_not_its_spelling() {
    // REGRESSION. C# has two spellings that are not part of the name (§6.4.3): the
    // verbatim prefix (`@x` IS the identifier `x`, written that way only to escape a
    // keyword collision) and Unicode escapes (`a` is `a`). Keying operands on the
    // raw token text made `int @x = 1; return x;` two distinct operands, so Halstead
    // vocabulary and volume tracked spelling rather than symbols.
    let halstead = |source: &str| {
        let a = analyze_clean(source);
        mehen_report::metrics_json::halstead(&a.root.metrics)
    };
    let plain = halstead("class C { static int F() { int a = 1; return a; } }");
    for spelling in [
        // verbatim prefix on the declaration, plain at the use
        "class C { static int F() { int @a = 1; return a; } }",
        // `\uXXXX` (4 hex digits)
        "class C { static int F() { int \\u0061 = 1; return a; } }",
        // `\UXXXXXXXX` (8) — the other width the grammar's `UnicodeEscape` admits
        "class C { static int F() { int \\U00000061 = 1; return a; } }",
    ] {
        let escaped = halstead(spelling);
        assert_eq!(
            escaped.n2, plain.n2,
            "one name, one distinct operand: {spelling}"
        );
        assert_eq!(escaped.volume, plain.volume, "and one volume: {spelling}");
    }
}

#[test]
fn distinct_names_and_literal_forms_still_stay_distinct() {
    // The guard on the normalization above: it must collapse *spellings of one name*,
    // never two names, and must not touch non-identifier operands — for a literal the
    // spelling IS the value, so `1`, `1L`, and `0x1` are genuinely three operands.
    let halstead = |source: &str| {
        let a = analyze_clean(source);
        mehen_report::metrics_json::halstead(&a.root.metrics)
    };
    let two_names = halstead("class C { static int F(int x, int y) => x + y; }");
    let one_name = halstead("class C { static int F(int x) => x + x; }");
    assert!(
        two_names.n2 > one_name.n2,
        "two parameters are two operands"
    );

    let mixed = halstead("class C { static long F() => 1 + 1L + 0x1; }");
    let same = halstead("class C { static long F() => 1 + 1 + 1; }");
    assert!(
        mixed.n2 > same.n2,
        "three literal forms are three operands, not one"
    );
}
