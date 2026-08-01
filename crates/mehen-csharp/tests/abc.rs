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
