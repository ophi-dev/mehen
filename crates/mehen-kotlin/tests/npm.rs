// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

//! NPM tests for the tree-sitter-kotlin walker.

use mehen_core::{AnalysisConfig, Language, LanguageAnalyzer, SourceFile};
use mehen_kotlin::KotlinAnalyzer;

fn analyze(source: &str) -> mehen_core::LanguageAnalysis {
    let mut text = source.trim_end().trim_matches('\n').to_string();
    text.push('\n');
    let analyzer = KotlinAnalyzer::new();
    let file = SourceFile::new("foo.kt".into(), Language::Kotlin, text);
    analyzer.analyze(&file, &AnalysisConfig::default()).unwrap()
}

#[test]
fn kotlin_npm_counts_visibility_modifiers() {
    let a = analyze(
        "class C {
             fun a() {}
             public fun b() {}
             private fun c() {}
             protected fun d() {}
             internal fun e() {}
         }",
    );
    let npm = mehen_report::metrics_json::npm(&a.root.metrics);
    // public: a, b. non-public: c, d, e.
    insta::assert_json_snapshot!(
        npm,
        @r#"
    {
      "classes": 2.0,
      "interfaces": 0.0,
      "class_methods": 5.0,
      "interface_methods": 0.0,
      "classes_average": 0.4,
      "interfaces_average": null,
      "total": 2.0,
      "total_methods": 5.0,
      "average": 0.4
    }
    "#
    );
}

#[test]
fn kotlin_npm_routes_interface_methods_to_interface_counters() {
    // tree-sitter-kotlin parses `class` and `interface` into the same
    // `class_declaration` node; only the leading keyword child
    // distinguishes them. Interface methods must land in the
    // interface_methods / interfaces counters, not class_methods /
    // classes.
    let a = analyze(
        "interface Foo {
             fun a()
             fun b(): Int
         }

         class Bar {
             fun c() {}
             fun d() {}
         }",
    );
    let npm = mehen_report::metrics_json::npm(&a.root.metrics);
    insta::assert_json_snapshot!(
        npm,
        @r#"
    {
      "classes": 2.0,
      "interfaces": 2.0,
      "class_methods": 2.0,
      "interface_methods": 2.0,
      "classes_average": 1.0,
      "interfaces_average": 1.0,
      "total": 4.0,
      "total_methods": 4.0,
      "average": 1.0
    }
    "#
    );
}

#[test]
fn kotlin_npm_counts_secondary_constructors() {
    let a = analyze(
        "class C {
             constructor()
             private constructor(x: Int)
             internal constructor(y: String)
             fun visible() {}
         }",
    );
    let npm = mehen_report::metrics_json::npm(&a.root.metrics);
    // public: default-visible constructor and visible().
    // non-public: private/internal secondary constructors.
    insta::assert_json_snapshot!(
        npm,
        @r#"
    {
      "classes": 2.0,
      "interfaces": 0.0,
      "class_methods": 4.0,
      "interface_methods": 0.0,
      "classes_average": 0.5,
      "interfaces_average": null,
      "total": 2.0,
      "total_methods": 4.0,
      "average": 0.5
    }
    "#
    );
}

#[test]
fn kotlin_npm_counts_property_accessors() {
    let a = analyze(
        "class C {
             var x: Int = 0
                 get() = field
                 private set(value) { field = value }

             private var hidden: Int = 0
                 get() = field
                 set(value) { field = value }
         }

         interface I {
             val y: Int
                 get() = 1
         }",
    );
    let npm = mehen_report::metrics_json::npm(&a.root.metrics);
    // class C -> public getter + private setter, plus two private
    // accessors inheriting from private property visibility.
    // interface I -> public getter.
    insta::assert_json_snapshot!(
        npm,
        @r#"
    {
      "classes": 1.0,
      "interfaces": 1.0,
      "class_methods": 4.0,
      "interface_methods": 1.0,
      "classes_average": 0.25,
      "interfaces_average": 1.0,
      "total": 2.0,
      "total_methods": 5.0,
      "average": 0.4
    }
    "#
    );
}

/// Regression: a method in an enum constant's anonymous body
/// (`A { fun local() {} }`) belongs to that anonymous subclass, not the
/// enum — no space is opened for the entry, so it must not be counted as a
/// method of the enclosing enum. Only the enum's own `shared` counts.
#[test]
fn kotlin_npm_excludes_enum_entry_body_members() {
    let a = analyze(
        "enum class E {
             A {
                 fun local() {}
             };

             fun shared() {}
         }",
    );
    let npm = mehen_report::metrics_json::npm(&a.root.metrics);
    assert_eq!(
        npm.class_methods, 1.0,
        "only the enum's own `shared` counts"
    );
    assert_eq!(npm.total_methods, 1.0);
}

/// Regression: a *real* nested class inside an enum-entry body must still
/// own its members. The `in_enum_entry` suppression (which keeps the entry's
/// own direct members off the enum) is cleared once a real class-like space
/// opens, so `class Inner { fun m() {} }` inside entry `A` counts `m` on
/// `Inner` — while the entry's direct `fun direct` does not count on the enum.
#[test]
fn kotlin_npm_counts_real_nested_class_inside_enum_entry() {
    let a = analyze(
        "enum class E {
             A {
                 class Inner { fun m() {} }
                 fun direct() {}
             };

             fun shared() {}
         }",
    );
    let npm = mehen_report::metrics_json::npm(&a.root.metrics);
    // `Inner.m` (on the nested class) + `E.shared` (on the enum) = 2.
    // `E.direct` (the entry's own method) is NOT attributed to the enum.
    assert_eq!(npm.total_methods, 2.0);
}

/// Regression: an object literal (`object { … }`) is an anonymous class
/// whose body opens no metric space, so its members must not be attributed
/// to the lexically-enclosing class. Only `C.outer` counts on `C`; the
/// object literal's `inner` does not.
#[test]
fn kotlin_npm_excludes_object_literal_body_members() {
    let a = analyze(
        "class C {
             fun outer() {}
             val o = object {
                 fun inner() {}
             }
         }",
    );
    let npm = mehen_report::metrics_json::npm(&a.root.metrics);
    assert_eq!(npm.class_methods, 1.0, "only `C.outer` counts");
    assert_eq!(npm.total_methods, 1.0);
}

/// Regression: a property accessor (`get`/`set`) inside an anonymous object
/// literal in a class property's initializer belongs to that anonymous
/// subclass, not the enclosing class. The accessor owner is threaded via
/// `property_visibility` (separate from the `in_class_member` gate that
/// suppresses ordinary members), so without clearing it inside an anonymous
/// body the inner getter was recorded on the enclosing class's NPM.
#[test]
fn kotlin_npm_excludes_object_literal_accessor() {
    let a = analyze(
        "class C {
             val o = object {
                 val p: Int = 0
                     get() = field
             }
         }",
    );
    let npm = mehen_report::metrics_json::npm(&a.root.metrics);
    assert_eq!(
        npm.class_methods, 0.0,
        "the object-literal's getter must not count on C"
    );
    assert_eq!(npm.total_methods, 0.0);
}

/// Sanity counterpart to the above: a *real* class-body property accessor
/// must still be counted as a class method. (Guards against the
/// anonymous-body fix over-suppressing genuine accessors.)
#[test]
fn kotlin_npm_counts_real_class_body_accessor() {
    let a = analyze(
        "class C {
             val p: Int = 0
                 get() = field
         }",
    );
    let npm = mehen_report::metrics_json::npm(&a.root.metrics);
    assert_eq!(npm.class_methods, 1.0, "C's own getter counts");
    assert_eq!(npm.total_methods, 1.0);
}
