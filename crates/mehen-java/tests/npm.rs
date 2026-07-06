// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! NPM (number of public methods) tests for the ANTLR Java walker.
//!
//! Java visibility: a class method with no access modifier is package-private
//! (NOT public); only an explicit `public` method counts toward NPM.
//! `protected`/`private` are non-public. Interface methods are implicitly
//! public.

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
fn only_explicitly_public_class_methods_count() {
    // `pub` public (1), `prot` protected, `priv` private, `pkg` package-private
    // → public NPM = 1, total methods = 4.
    let a = analyze(
        "class C {
             public void pub() {}
             protected void prot() {}
             private void priv() {}
             void pkg() {}
         }",
    );
    let npm = mehen_report::metrics_json::npm(&a.root.metrics);
    insta::assert_json_snapshot!(npm, @r#"
    {
      "classes": 1.0,
      "interfaces": 0.0,
      "class_methods": 4.0,
      "interface_methods": 0.0,
      "classes_average": 0.25,
      "interfaces_average": null,
      "total": 1.0,
      "total_methods": 4.0,
      "average": 0.25
    }
    "#);
}

#[test]
fn generic_methods_and_constructors_count() {
    // Regression (audit): generic methods/constructors reach the walker
    // through genericMethodDeclaration/genericConstructorDeclaration wrappers.
    // Both public members must count toward NPM.
    let a = analyze(
        "class C {
             public <T> T identity(T x) { return x; }
             public <T> C(T seed) {}
         }",
    );
    let npm = mehen_report::metrics_json::npm(&a.root.metrics);
    insta::assert_json_snapshot!(npm, @r#"
    {
      "classes": 2.0,
      "interfaces": 0.0,
      "class_methods": 2.0,
      "interface_methods": 0.0,
      "classes_average": 1.0,
      "interfaces_average": null,
      "total": 2.0,
      "total_methods": 2.0,
      "average": 1.0
    }
    "#);
}

#[test]
fn generic_interface_methods_count() {
    // Regression (audit): a generic interface method reaches the walker via
    // genericInterfaceMethodDeclaration → interfaceCommonBodyDeclaration and
    // must count exactly once toward interface NPM.
    let a = analyze(
        "interface I {
             int plain();
             <T> T generic(T x);
         }",
    );
    let npm = mehen_report::metrics_json::npm(&a.root.metrics);
    insta::assert_json_snapshot!(npm, @r#"
    {
      "classes": 0.0,
      "interfaces": 2.0,
      "class_methods": 0.0,
      "interface_methods": 2.0,
      "classes_average": null,
      "interfaces_average": 1.0,
      "total": 2.0,
      "total_methods": 2.0,
      "average": 1.0
    }
    "#);
}

#[test]
fn interface_methods_are_public() {
    let a = analyze(
        "interface I {
             void m();
             default int d() { return 2; }
         }",
    );
    let npm = mehen_report::metrics_json::npm(&a.root.metrics);
    insta::assert_json_snapshot!(npm, @r#"
    {
      "classes": 0.0,
      "interfaces": 2.0,
      "class_methods": 0.0,
      "interface_methods": 2.0,
      "classes_average": null,
      "interfaces_average": 1.0,
      "total": 2.0,
      "total_methods": 2.0,
      "average": 1.0
    }
    "#);
}

#[test]
fn annotation_elements_are_public_interface_methods() {
    // Regression (PR #160 review): annotation elements (`String value();`)
    // reach the walker via annotationMethodRest and are implicitly-public
    // interface-like methods — they must count toward interface NPM.
    let a = analyze("@interface Ann { String value(); int count(); }");
    let npm = mehen_report::metrics_json::npm(&a.root.metrics);
    insta::assert_json_snapshot!(npm, @r#"
    {
      "classes": 0.0,
      "interfaces": 2.0,
      "class_methods": 0.0,
      "interface_methods": 2.0,
      "classes_average": null,
      "interfaces_average": 1.0,
      "total": 2.0,
      "total_methods": 2.0,
      "average": 1.0
    }
    "#);
}

#[test]
fn compact_record_constructor_counts_as_public_method() {
    // Regression (PR #160 review): a compact record constructor is a direct
    // `compactConstructorDeclaration` child of `recordBody` (not wrapped in
    // `classBodyDeclaration`), so the member position must be seeded from
    // `recordBody`. Its visibility comes from its own modifiers.
    let a = analyze("record R(int x) { public R { } }");
    let npm = mehen_report::metrics_json::npm(&a.root.metrics);
    insta::assert_json_snapshot!(npm, @r#"
    {
      "classes": 1.0,
      "interfaces": 0.0,
      "class_methods": 1.0,
      "interface_methods": 0.0,
      "classes_average": 1.0,
      "interfaces_average": null,
      "total": 1.0,
      "total_methods": 1.0,
      "average": 1.0
    }
    "#);
}

#[test]
fn modifierless_compact_constructor_inherits_record_visibility() {
    // Regression (PR #160 review): Java gives a modifier-less compact canonical
    // constructor the RECORD's access level. The compact ctor is reached under
    // the (modifier-less) `recordBody`, so it must inherit the record's
    // visibility — threaded via `enclosing_record_public` — not the record-body
    // default (always package-private). A `public record` with the common
    // modifier-less compact ctor therefore has NPM = 1.
    let public_top = analyze("public record R(int x) { R { } }");
    let pt =
        serde_json::to_value(mehen_report::metrics_json::npm(&public_top.root.metrics)).unwrap();
    assert_eq!(
        pt["total"],
        serde_json::json!(1.0),
        "a modifier-less compact ctor in a public (top-level) record is public"
    );
    // Nested public record: visibility comes through the classBodyDeclaration
    // wrapper. The outer class contributes no methods; only the record's ctor.
    let public_nested = analyze("class C { public record R(int x) { R { } } }");
    let pn =
        serde_json::to_value(mehen_report::metrics_json::npm(&public_nested.root.metrics)).unwrap();
    assert_eq!(
        pn["total"],
        serde_json::json!(1.0),
        "a modifier-less compact ctor in a nested public record is public"
    );
    // Package-private record → its modifier-less compact ctor is NOT public.
    let pkg = analyze("record R(int x) { R { } }");
    let pk = serde_json::to_value(mehen_report::metrics_json::npm(&pkg.root.metrics)).unwrap();
    assert_eq!(
        pk["total"],
        serde_json::json!(0.0),
        "a modifier-less compact ctor in a package-private record is not public"
    );
    // An explicit modifier on the compact ctor still wins over the record's.
    let explicit_priv = analyze("public record R(int x) { private R { } }");
    let ep =
        serde_json::to_value(mehen_report::metrics_json::npm(&explicit_priv.root.metrics)).unwrap();
    assert_eq!(
        ep["total"],
        serde_json::json!(0.0),
        "an explicit private compact ctor overrides the record's public access"
    );
}

#[test]
fn anonymous_class_body_methods_are_not_enclosing_members() {
    // Regression (PR #160 review): a method in an anonymous class expression
    // (`new Runnable() { void run() {} }`) belongs to the anonymous subclass,
    // not the enclosing class, so it must NOT count toward the enclosing
    // class's NPM.
    let a = analyze("class C { Runnable r = new Runnable() { public void run() {} }; }");
    let npm = mehen_report::metrics_json::npm(&a.root.metrics);
    insta::assert_json_snapshot!(npm, @r#"
    {
      "classes": 0.0,
      "interfaces": 0.0,
      "class_methods": 0.0,
      "interface_methods": 0.0,
      "classes_average": null,
      "interfaces_average": null,
      "total": 0.0,
      "total_methods": 0.0,
      "average": null
    }
    "#);
}

#[test]
fn enum_constant_body_methods_are_not_enum_members() {
    // Regression (PR #160 review): a method inside a constant-specific enum
    // body belongs to that constant's anonymous subclass, not the enum, so it
    // must NOT count toward the enum's NPM. The enum declares no methods of its
    // own here.
    let a = analyze(
        "enum E {
             A {
                 public void m() {}
             };
         }",
    );
    let npm = mehen_report::metrics_json::npm(&a.root.metrics);
    insta::assert_json_snapshot!(npm, @r#"
    {
      "classes": 0.0,
      "interfaces": 0.0,
      "class_methods": 0.0,
      "interface_methods": 0.0,
      "classes_average": null,
      "interfaces_average": null,
      "total": 0.0,
      "total_methods": 0.0,
      "average": null
    }
    "#);
}

#[test]
fn interface_nested_record_compact_ctor_is_public() {
    // Regression (PR #160 review): after class-like spaces open at their
    // wrapper (round 29), member visibility must be resolved from the container
    // enclosing the wrapper, NOT the just-opened type space. An interface's
    // members are implicitly public, so a modifier-less compact canonical
    // constructor of an interface-nested record inherits public access → NPM 1.
    let iface = analyze("interface I { record R(int x) { R {} } }");
    let i = serde_json::to_value(mehen_report::metrics_json::npm(&iface.root.metrics)).unwrap();
    assert_eq!(
        i["total"],
        serde_json::json!(1.0),
        "an interface-nested record's modifier-less compact ctor is public"
    );
    // Control: the same nested in a class is package-private (not public).
    let cls = analyze("class C { record R(int x) { R {} } }");
    let c = serde_json::to_value(mehen_report::metrics_json::npm(&cls.root.metrics)).unwrap();
    assert_eq!(
        c["total"],
        serde_json::json!(0.0),
        "a class-nested record's modifier-less compact ctor is package-private"
    );
}

#[test]
fn anon_body_nested_record_preserves_record_visibility() {
    // Regression (PR #160 review): the `in_anon_body` early return in
    // member_propagation must not discard visibility a nested type needs. A
    // `public record` inside an anonymous class body must keep its `public` so
    // its modifier-less compact canonical constructor is counted public — even
    // though the anon body's OWN members are not attributed to any enclosing
    // space. `new Object(){ public record R(int x) { R {} } }` → NPM 1.
    let public_rec =
        analyze("class C { Object o = new Object(){ public record R(int x) { R {} } }; }");
    let pr =
        serde_json::to_value(mehen_report::metrics_json::npm(&public_rec.root.metrics)).unwrap();
    assert_eq!(
        pr["total"],
        serde_json::json!(1.0),
        "a public record's compact ctor stays public inside an anon body"
    );
    // A package-private record in an anon body → its compact ctor is not public.
    let pkg_rec = analyze("class C { Object o = new Object(){ record R(int x) { R {} } }; }");
    let kr = serde_json::to_value(mehen_report::metrics_json::npm(&pkg_rec.root.metrics)).unwrap();
    assert_eq!(kr["total"], serde_json::json!(0.0));
    // Guard: the anon body's OWN public method must still NOT count toward the
    // enclosing class's NPM (it belongs to the anonymous subclass).
    let own_method = analyze("class C { Object o = new Object(){ public void m() {} }; }");
    let om =
        serde_json::to_value(mehen_report::metrics_json::npm(&own_method.root.metrics)).unwrap();
    assert_eq!(
        om["total"],
        serde_json::json!(0.0),
        "an anonymous class's own method must not count toward the enclosing NPM"
    );
}
