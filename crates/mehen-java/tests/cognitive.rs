// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! Cognitive complexity tests for the ANTLR Java walker (SonarSource rules).
//!
//! Nesting increments (`+1` plus the current nesting level): `if`, loops,
//! `switch`, `catch`, and the ternary. Flat `+1`: `else`/`else if`, labeled
//! `break`/`continue`. Sequences of like boolean operators collapse. `else if`
//! does not add a nesting level.

use mehen_core::{AnalysisConfig, Language, LanguageAnalyzer, SourceFile};
use mehen_java::JavaAnalyzer;

fn analyze(source: &str) -> mehen_core::LanguageAnalysis {
    let mut text = source.trim_end().trim_matches('\n').to_string();
    text.push('\n');
    let analyzer = JavaAnalyzer::new();
    let file = SourceFile::new("Foo.java".into(), Language::Java, text);
    analyzer.analyze(&file, &AnalysisConfig::default()).unwrap()
}

#[test]
fn nested_structures_accumulate_nesting() {
    // for(+1) → if(+2) → while(+3) = 6.
    let a = analyze(
        "class C {
             void f(int[] xs) {
                 for (int x : xs) {
                     if (x > 0) {
                         while (x > 0) { x--; }
                     }
                 }
             }
         }",
    );
    let cog = mehen_report::metrics_json::cognitive(&a.root.metrics);
    insta::assert_json_snapshot!(cog, @r###"
    {
      "sum": 6.0,
      "average": 6.0,
      "min": 0.0,
      "max": 6.0
    }
    "###);
}

#[test]
fn boolean_sequence_collapses_like_operators() {
    // `if`(+1) then `a && b || c`: one `&&` run (+1) and one `||` run (+1) = 3.
    let a = analyze(
        "class C {
             boolean check(boolean a, boolean b, boolean c) {
                 if (a && b || c) { return true; }
                 return false;
             }
         }",
    );
    let cog = mehen_report::metrics_json::cognitive(&a.root.metrics);
    insta::assert_json_snapshot!(cog, @r###"
    {
      "sum": 3.0,
      "average": 3.0,
      "min": 0.0,
      "max": 3.0
    }
    "###);
}

#[test]
fn mixed_boolean_operators_count_each_run() {
    // `if`(+1) then `a && b || c && d`: three like-operator runs — the first
    // `&&` (+1), the `||` (+1), and the second `&&` (+1) — because switching
    // operator ends a run and switching back starts a new one. Total = 4.
    let a = analyze(
        "class C {
             boolean check(boolean a, boolean b, boolean c, boolean d) {
                 if (a && b || c && d) { return true; }
                 return false;
             }
         }",
    );
    let cog = mehen_report::metrics_json::cognitive(&a.root.metrics);
    insta::assert_json_snapshot!(cog, @r###"
    {
      "sum": 4.0,
      "average": 4.0,
      "min": 0.0,
      "max": 4.0
    }
    "###);
}

#[test]
fn else_if_does_not_add_nesting() {
    // if(+1), else if → flat else(+1) + the if is an else-branch so no
    // nesting, else(+1) = 3 total.
    let a = analyze(
        "class C {
             int f(int x) {
                 if (x > 2) { return 2; }
                 else if (x > 1) { return 1; }
                 else { return 0; }
             }
         }",
    );
    let cog = mehen_report::metrics_json::cognitive(&a.root.metrics);
    insta::assert_json_snapshot!(cog, @r###"
    {
      "sum": 3.0,
      "average": 3.0,
      "min": 0.0,
      "max": 3.0
    }
    "###);
}

#[test]
fn parentheses_do_not_break_boolean_run_collapse() {
    // Regression (PR #160 review): a parenthesized boolean sub-expression
    // (`primary: '(' expression ')'`) must stay in the same boolean run. All
    // three forms below are a single `&&` run → cognitive 2 (if=1, one run=1).
    for src in [
        "class C { boolean f(boolean a, boolean b, boolean c) { if ((a && b) && c) return true; return false; } }",
        "class C { boolean f(boolean a, boolean b, boolean c) { if (a && (b && c)) return true; return false; } }",
        "class C { boolean f(boolean a, boolean b, boolean c) { if (a && b && c) return true; return false; } }",
    ] {
        let a = analyze(src);
        let cog =
            serde_json::to_value(mehen_report::metrics_json::cognitive(&a.root.metrics)).unwrap();
        assert_eq!(
            cog["sum"],
            serde_json::json!(2.0),
            "boolean run should collapse for: {src}"
        );
    }
}

#[test]
fn negation_breaks_boolean_run() {
    // Regression (PR #160 review): a prefix `!` negation breaks a same-operator
    // boolean run (SonarSource rule; matches the Kotlin walker). For
    // `a && !b && c` the two `&&` do NOT collapse — the `!` on `b` separates
    // them: if(+1), first `&&`(+1), second `&&` after the negation(+1) = 3.
    let a = analyze(
        "class C {
             boolean f(boolean a, boolean b, boolean c) {
                 if (a && !b && c) return true;
                 return false;
             }
         }",
    );
    let cog = mehen_report::metrics_json::cognitive(&a.root.metrics);
    insta::assert_json_snapshot!(cog, @r###"
    {
      "sum": 3.0,
      "average": 3.0,
      "min": 0.0,
      "max": 3.0
    }
    "###);
    // Control: without the negation, the two `&&` collapse into one run → 2.
    let plain = analyze(
        "class C {
             boolean f(boolean a, boolean b, boolean c) {
                 if (a && b && c) return true;
                 return false;
             }
         }",
    );
    let pj =
        serde_json::to_value(mehen_report::metrics_json::cognitive(&plain.root.metrics)).unwrap();
    assert_eq!(pj["sum"], serde_json::json!(2.0));
}

#[test]
fn parenthesized_negation_still_breaks_boolean_run() {
    // Regression (PR #160 review): the negation that breaks a boolean run may
    // be wrapped in transparent parentheses (`primary: '(' expression ')'`), so
    // `a && (!b) && c` and `a && ((!b)) && c` must break the run just like the
    // bare `a && !b && c` → cognitive 3.
    for src in [
        "class C { boolean f(boolean a, boolean b, boolean c) { if (a && (!b) && c) return true; return false; } }",
        "class C { boolean f(boolean a, boolean b, boolean c) { if (a && ((!b)) && c) return true; return false; } }",
    ] {
        let a = analyze(src);
        let cog =
            serde_json::to_value(mehen_report::metrics_json::cognitive(&a.root.metrics)).unwrap();
        assert_eq!(
            cog["sum"],
            serde_json::json!(3.0),
            "paren negation should break the run: {src}"
        );
    }
    // Guard: a parenthesized *non-negation* operand still collapses (no `!`),
    // and `!=` inside a paren operand is not mistaken for `!`.
    for src in [
        "class C { boolean f(boolean a, boolean b, boolean c) { if (a && (b) && c) return true; return false; } }",
        "class C { boolean f(boolean a, int b, boolean c) { if (a && (b != 0) && c) return true; return false; } }",
    ] {
        let a = analyze(src);
        let cog =
            serde_json::to_value(mehen_report::metrics_json::cognitive(&a.root.metrics)).unwrap();
        assert_eq!(
            cog["sum"],
            serde_json::json!(2.0),
            "no spurious run break: {src}"
        );
    }
}

#[test]
fn leading_negation_does_not_break_boolean_run() {
    // Regression (PR #160 review): a negation only breaks a same-operator run
    // when it sits *between* two like operators (`a && !b && c` = 3). A
    // *leading* negation — before the first operator, i.e. the left operand of
    // the innermost `&&` in the left-associative chain `((!a && b) && c)` — has
    // no preceding operator to split, so it must NOT break the run (matches the
    // Kotlin walker's order-sensitive `not_operator` behavior). Each of these
    // is a single `&&` run → cognitive 2 (if=1, one run=1).
    for src in [
        "class C { boolean f(boolean a, boolean b, boolean c) { if (!a && b && c) return true; return false; } }",
        "class C { boolean f(boolean a, boolean b, boolean c) { if ((!a) && b && c) return true; return false; } }",
    ] {
        let a = analyze(src);
        let cog =
            serde_json::to_value(mehen_report::metrics_json::cognitive(&a.root.metrics)).unwrap();
        assert_eq!(
            cog["sum"],
            serde_json::json!(2.0),
            "a leading negation must not break the run: {src}"
        );
    }
    // Guard: a negation on a *later* operand still breaks the run. `!a && !b && c`
    // — the first `!a` is leading (no split), but the second `!b` sits between
    // the two `&&` and splits them → two runs → cognitive 3.
    let mid = analyze(
        "class C { boolean f(boolean a, boolean b, boolean c) { if (!a && !b && c) return true; return false; } }",
    );
    let mj =
        serde_json::to_value(mehen_report::metrics_json::cognitive(&mid.root.metrics)).unwrap();
    assert_eq!(
        mj["sum"],
        serde_json::json!(3.0),
        "a non-leading negation still breaks the run"
    );
}

#[test]
fn negation_in_parenthesized_continuation_breaks_boolean_run() {
    // Regression (PR #160 review): a negation breaks a same-operator run
    // whenever a like operator *precedes* it in the FLATTENED run — including
    // when the negated operand starts a parenthesized continuation of the run.
    // `&&` is left-associative, so `a && !b && c` parses as `((a && !b) && c)`
    // (the `!b` is a right operand — caught by `has_negated_operand`), but
    // `a && (!b && c)` parses as `a && (…)` where `!b` is the LEFT operand of
    // the parenthesized sub-run. Parentheses are transparent to the run, so the
    // flattened form is the same `a && !b && c` and it must score 3, not 2.
    for src in [
        "class C { boolean f(boolean a, boolean b, boolean c) { if (a && (!b && c)) return true; return false; } }",
        "class C { boolean f(boolean a, boolean b, boolean c) { if (a && ((!b && c))) return true; return false; } }",
        "class C { boolean f(boolean a, boolean b, boolean c) { if ((a && (!b && c))) return true; return false; } }",
        "class C { boolean f(boolean a, boolean b, boolean c) { if (a || (!b || c)) return true; return false; } }",
    ] {
        let a = analyze(src);
        let cog =
            serde_json::to_value(mehen_report::metrics_json::cognitive(&a.root.metrics)).unwrap();
        assert_eq!(
            cog["sum"],
            serde_json::json!(3.0),
            "a negation continuing a run (in parens) must break it: {src}"
        );
    }
    // Guard: a *globally leading* negation inside parentheses still does NOT
    // break the run — nothing precedes it in the flattened run.
    // `(!b) && a && c` is one `&&` run → 2.
    let leading = analyze(
        "class C { boolean f(boolean a, boolean b, boolean c) { if ((!b) && a && c) return true; return false; } }",
    );
    let lj =
        serde_json::to_value(mehen_report::metrics_json::cognitive(&leading.root.metrics)).unwrap();
    assert_eq!(
        lj["sum"],
        serde_json::json!(2.0),
        "a leading negation (even parenthesized) does not break the run"
    );
}

#[test]
fn operator_expression_resets_boolean_run() {
    // Regression (PR #160 review): only *transparent* wrappers (parens/bare
    // operands) preserve a boolean run; an expression with its own operator
    // (here `==`) is a distinct boolean context. For `a && ((b && c) == d)`
    // the inner `b && c` must NOT collapse with the outer `&&`:
    //   if(+1), outer `&&`(+1), inner `&&` (fresh run after `==`)(+1) = 3.
    let a = analyze(
        "class C {
             boolean f(int a, int b, int c, int d) {
                 if (a > 0 && ((b > 0 && c > 0) == (d > 0))) return true;
                 return false;
             }
         }",
    );
    let cog = mehen_report::metrics_json::cognitive(&a.root.metrics);
    insta::assert_json_snapshot!(cog, @r###"
    {
      "sum": 3.0,
      "average": 3.0,
      "min": 0.0,
      "max": 3.0
    }
    "###);
}

#[test]
fn lambda_in_plain_constructor_argument_inherits_method_depth() {
    // Regression (PR #160 review): `new Foo(() -> …)` routes through
    // `classCreatorRest: arguments classBody?`, but only the optional
    // `classBody` is an anonymous body — the `arguments` (a plain constructor
    // call) is not. A lambda passed as a constructor argument must inherit the
    // enclosing method's depth, exactly like a lambda passed to a method call.
    let ctor = analyze("class C { void m() { new Foo(() -> { if (x) {} }); } }");
    let call = analyze("class C { void m() { bar(() -> { if (x) {} }); } }");
    let cc =
        serde_json::to_value(mehen_report::metrics_json::cognitive(&ctor.root.metrics)).unwrap();
    let ca =
        serde_json::to_value(mehen_report::metrics_json::cognitive(&call.root.metrics)).unwrap();
    assert_eq!(
        cc["sum"], ca["sum"],
        "a lambda in a constructor argument must score like one in a method-call argument"
    );
    assert_eq!(cc["sum"], serde_json::json!(2.0));
}

#[test]
fn lambda_inside_anonymous_class_method_inherits_method_depth() {
    // Regression (PR #160 review): `in_anon_body` is a subtree-wide flag, but a
    // lambda nested *inside* an anonymous class's method is enclosed by the
    // method (a function), not the anon body — so it must inherit the method's
    // cognitive depth. The lambda's `if` scores the same whether the method is
    // in an anonymous class or a plain class.
    let anon = analyze(
        "class C { void outer() { new Runnable() { public void run() { Runnable r = () -> { if (x) {} }; } }; } }",
    );
    let plain = analyze("class C { void run() { Runnable r = () -> { if (x) {} }; } }");
    let a =
        serde_json::to_value(mehen_report::metrics_json::cognitive(&anon.root.metrics)).unwrap();
    let p =
        serde_json::to_value(mehen_report::metrics_json::cognitive(&plain.root.metrics)).unwrap();
    assert_eq!(
        a["sum"], p["sum"],
        "a lambda inside an anon-class method must inherit the method's depth"
    );
    assert_eq!(a["sum"], serde_json::json!(2.0));
}

#[test]
fn method_in_anonymous_class_does_not_inherit_outer_depth() {
    // Regression (PR #160 review): an anonymous class opens no metric space
    // (tracked only via `in_anon_body`), so the ancestor scan can't see a
    // class boundary. A method in `new Runnable(){ void run(){…} }` inside
    // `outer()` must still start at the baseline depth, not inherit `outer`'s.
    let nested =
        analyze("class C { void outer() { new Runnable() { public void run() { if (x) {} } }; } }");
    let flat =
        analyze("class C { Runnable r = new Runnable() { public void run() { if (x) {} } }; }");
    let n =
        serde_json::to_value(mehen_report::metrics_json::cognitive(&nested.root.metrics)).unwrap();
    let f =
        serde_json::to_value(mehen_report::metrics_json::cognitive(&flat.root.metrics)).unwrap();
    assert_eq!(
        n["sum"], f["sum"],
        "an anonymous-class method must not inherit the enclosing method's depth"
    );
    assert_eq!(n["sum"], serde_json::json!(1.0));
}

#[test]
fn anonymous_class_body_initializer_does_not_inherit_enclosing_nesting() {
    // Regression (PR #160 review): an anonymous class body (`new X() { … }`) is
    // a fresh class scope but opens no metric space, so — like a named class
    // (which resets via `enter_class_cognitive`) — its class-body-level code
    // (an instance initializer block) must not inherit the enclosing `if`'s
    // nesting. `if (a) { new Object() { { if (b) {} } }; }`:
    //   if (a) → +1; the anon body's initializer `if (b)` is a fresh scope → +1
    // = 2, not 3.
    let nested = analyze("class C { void m() { if (a) { new Object() { { if (b) {} } }; } } }");
    let flat = analyze("class C { void m() { new Object() { { if (b) {} } }; } }");
    let n =
        serde_json::to_value(mehen_report::metrics_json::cognitive(&nested.root.metrics)).unwrap();
    let f =
        serde_json::to_value(mehen_report::metrics_json::cognitive(&flat.root.metrics)).unwrap();
    assert_eq!(
        f["sum"],
        serde_json::json!(1.0),
        "anon initializer `if` is a fresh scope"
    );
    assert_eq!(
        n["sum"],
        serde_json::json!(2.0),
        "outer if (1) + anon initializer if at baseline (1), not nested (2)"
    );
}

#[test]
fn class_body_initializer_does_not_inherit_enclosing_nesting() {
    // Regression (PR #160 review): a class-like scope resets the cognitive
    // context, so code that runs *directly* in a class body (an instance
    // initializer block) does not inherit the enclosing statement's nesting.
    // Methods reset via `enter_function_cognitive`; class-body code opens no
    // function space, so the class-open must reset it. Here a local class with
    // an initializer block is declared inside `if (a)`:
    //   if (a) → +1; the initializer's `if (b)` is a fresh scope → +1 = 2.
    // Without the reset the inner `if` would be scored nested (+2), giving 3.
    let nested = analyze("class C { void m() { if (a) { class L { { if (b) {} } } } } }");
    let flat = analyze("class C { void m() { class L { { if (b) {} } } } }");
    let n =
        serde_json::to_value(mehen_report::metrics_json::cognitive(&nested.root.metrics)).unwrap();
    let f =
        serde_json::to_value(mehen_report::metrics_json::cognitive(&flat.root.metrics)).unwrap();
    // The local class's initializer `if` scores the same (baseline 1) whether
    // or not the class is declared inside the outer `if`.
    assert_eq!(
        f["sum"],
        serde_json::json!(1.0),
        "initializer `if` is a fresh scope"
    );
    assert_eq!(
        n["sum"],
        serde_json::json!(2.0),
        "outer if (1) + initializer if at baseline (1), not nested (2)"
    );
}

#[test]
fn method_in_local_class_does_not_inherit_outer_depth() {
    // Regression (PR #160 review): a method in a local/anonymous class nested
    // in another method must NOT inherit the outer method's cognitive depth —
    // a class scope resets the baseline. `inner`'s `if` scores 1, matching the
    // same method declared without the enclosing method.
    let local = analyze("class C { void outer() { class L { void inner() { if (x) {} } } } }");
    let flat = analyze("class C { class L { void inner() { if (x) {} } } }");
    let l =
        serde_json::to_value(mehen_report::metrics_json::cognitive(&local.root.metrics)).unwrap();
    let f =
        serde_json::to_value(mehen_report::metrics_json::cognitive(&flat.root.metrics)).unwrap();
    assert_eq!(
        l["sum"], f["sum"],
        "a method in a local class must not inherit the enclosing method's depth"
    );
    assert_eq!(l["sum"], serde_json::json!(1.0));
}

#[test]
fn else_if_flag_does_not_leak_through_a_loop_body() {
    // Regression (PR #160 review): the `is_else_branch` flag must only flow
    // through a *transparent* wrapper statement toward an else-if, NOT through
    // a loop/switch/try in the else position. For `if (a) {} else while (c) if
    // (b) {}` the `while` body's `if (b)` is a genuinely nested `if`, not an
    // `else if`, so it must keep its cognitive nesting increment. If the flag
    // leaked, `if (b)` would be scored as an else-if (no nesting) and the score
    // would drop by its nesting contribution.
    let leaked = analyze(
        "class C { void m(int a, int c, int b) { if (a > 0) {} else while (c > 0) if (b > 0) {} } }",
    );
    let no_body_if =
        analyze("class C { void m(int a, int c) { if (a > 0) {} else while (c > 0) {} } }");
    let l =
        serde_json::to_value(mehen_report::metrics_json::cognitive(&leaked.root.metrics)).unwrap();
    let n = serde_json::to_value(mehen_report::metrics_json::cognitive(
        &no_body_if.root.metrics,
    ))
    .unwrap();
    // Adding the nested `if (b)` inside the else-branch loop must increase the
    // cognitive score — it is NOT an else-if and must not be suppressed.
    assert!(
        l["sum"].as_f64().unwrap() > n["sum"].as_f64().unwrap(),
        "the loop-body `if` must add cognitive nesting (not be treated as else-if): \
         with-if={} without-if={}",
        l["sum"],
        n["sum"]
    );
}

#[test]
fn braceless_if_in_else_if_then_branch_still_nests() {
    // Regression (PR #160 review): the `is_else_branch` flag must NOT leak from
    // an else-if node onto its *then*-branch. Here `else if (b > 0)` has a
    // braceless then-branch containing `if (d > 0)`, which is a genuine nested
    // `if` and must add nesting.
    //   if (a > 0)          -> +1
    //   else if (b > 0)     -> flat else +1 (the `if` is an else-branch: no nest)
    //     if (d > 0) ...    -> +2 (nested at level 1 inside the else-if body)
    // = 4. If the flag leaked, the inner `if` would be mis-tagged as an
    // else-branch and skip its nesting, giving 2.
    let a = analyze(
        "class C {
             int f(int a, int b, int d) {
                 if (a > 0) { return 1; }
                 else if (b > 0)
                     if (d > 0) return 2;
                 return 0;
             }
         }",
    );
    let cog = mehen_report::metrics_json::cognitive(&a.root.metrics);
    insta::assert_json_snapshot!(cog, @r###"
    {
      "sum": 4.0,
      "average": 4.0,
      "min": 0.0,
      "max": 4.0
    }
    "###);
}

#[test]
fn switch_expression_scores_like_switch_statement() {
    // Regression (audit): a switch *expression* (Java 14+) must get the same
    // cognitive nesting as the statement form. Here: switch expr(+1) then a
    // nested `if` in an arm at nesting 1 (+2) = 3.
    let expr = analyze(
        "class C {
             int f(int x) {
                 int y = switch (x) {
                     case 1 -> { if (x > 0) { yield 1; } yield 2; }
                     default -> 0;
                 };
                 return y;
             }
         }",
    );
    let stmt = analyze(
        "class C {
             int f(int x) {
                 switch (x) {
                     case 1: if (x > 0) { return 1; } return 2;
                     default: return 0;
                 }
             }
         }",
    );
    let e = mehen_report::metrics_json::cognitive(&expr.root.metrics);
    let s = mehen_report::metrics_json::cognitive(&stmt.root.metrics);
    let ej = serde_json::to_value(&e).unwrap();
    let sj = serde_json::to_value(&s).unwrap();
    assert_eq!(
        ej, sj,
        "switch expression and switch statement must score identically"
    );
    insta::assert_json_snapshot!(e, @r###"
    {
      "sum": 3.0,
      "average": 3.0,
      "min": 0.0,
      "max": 3.0
    }
    "###);
}

#[test]
fn nested_ternary_deepens_nesting() {
    // Regression (audit): a ternary nested in another ternary's operand is one
    // level deeper. `a>0 ? (b>0 ? 1 : 2) : 3`: outer ternary(+1 at level 0),
    // inner ternary(+2 at level 1) = 3.
    let a = analyze(
        "class C {
             int f(int a, int b) {
                 return a > 0 ? (b > 0 ? 1 : 2) : 3;
             }
         }",
    );
    let cog = mehen_report::metrics_json::cognitive(&a.root.metrics);
    insta::assert_json_snapshot!(cog, @r###"
    {
      "sum": 3.0,
      "average": 3.0,
      "min": 0.0,
      "max": 3.0
    }
    "###);
}

#[test]
fn catch_adds_nesting_increment() {
    // `catch`(+1). `try` itself adds nothing.
    let a = analyze(
        "class C {
             void f() {
                 try { risky(); } catch (Exception e) { }
             }
         }",
    );
    let cog = mehen_report::metrics_json::cognitive(&a.root.metrics);
    insta::assert_json_snapshot!(cog, @r###"
    {
      "sum": 1.0,
      "average": 1.0,
      "min": 0.0,
      "max": 1.0
    }
    "###);
}
