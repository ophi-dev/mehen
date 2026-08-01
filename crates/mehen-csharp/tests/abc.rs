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
