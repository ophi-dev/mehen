// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

use std::fmt::Write;

use mehen_core::{
    DiagnosticSeverity, DiffReport, Language, MetricKey, MetricSet, MetricSpace, MetricsReport,
    ParseDiagnostic, SpaceKind,
};

use crate::metrics_json::{
    Abc, Cognitive, Cyclomatic, Halstead, Loc, MetricsFamilies, Nargs, Nexits, Nom, Npa, Npm, Wmc,
};

/// Render a single-file metrics report as Markdown.
///
/// The output is structured for both human consumption (running
/// `mehen metrics file --format markdown` from a terminal) and for
/// CI artefacts (a docs build that wants to diff metric counts as
/// stable text). The same family object the JSON renderer publishes
/// drives the metric tables here, so JSON and Markdown stay in lock
/// step.
///
/// Layout:
/// 1. Title + file metadata block.
/// 2. Diagnostics callout (only emitted when at least one
///    diagnostic exists; severity is shown inline).
/// 3. Per-family metric tables (cyclomatic, cognitive, LOC,
///    Halstead, ABC, NArgs, NOM, NExits, NPA, NPM, WMC) — every
///    family is rendered, even when its scalar values are all
///    zero, so the absence of a bucket doesn't have to be inferred.
/// 4. Per-space breakdown for nested function / closure / class /
///    interface / impl / trait / enum spaces beneath the unit. Pure
///    "Unknown" or empty trees collapse to a single "no nested
///    spaces" line.
pub fn render_metrics_markdown(report: &MetricsReport) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# {}", report.path);
    let _ = writeln!(out);
    let _ = writeln!(out, "- language: `{}`", report.language);
    let _ = writeln!(out, "- backend: `{}`", report.analysis_backend.label());
    let _ = writeln!(out, "- schema: `{}`", report.schema_version);

    write_diagnostics(&mut out, &report.diagnostics);
    write_unit_metrics(&mut out, &report.root.metrics, report.language);
    write_nested_spaces(&mut out, &report.root.spaces, 0, report.language);

    out
}

fn write_diagnostics(out: &mut String, diagnostics: &[ParseDiagnostic]) {
    if diagnostics.is_empty() {
        return;
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "## Diagnostics");
    let _ = writeln!(out);
    let _ = writeln!(out, "| severity | code | message |");
    let _ = writeln!(out, "|---|---|---|");
    for d in diagnostics {
        let severity = match d.severity {
            DiagnosticSeverity::Warning => "warning",
            DiagnosticSeverity::Error => "error",
            DiagnosticSeverity::Fatal => "fatal",
        };
        let _ = writeln!(
            out,
            "| {} | `{}` | {} |",
            severity,
            d.code,
            escape_table_cell(&d.message),
        );
    }
}

fn write_unit_metrics(out: &mut String, metrics: &MetricSet, language: Language) {
    let _ = writeln!(out);
    let _ = writeln!(out, "## Metrics");

    if language == Language::Markdown {
        // The Markdown analyzer publishes a different metric family
        // (`markdown.*` keys covering documentation-specific
        // dimensions: LOC ratios, prose size, links, visuals,
        // maintainability, grounding, etc.). Pivoting through
        // `MetricsFamilies` would emit all-zero source-code tables
        // that don't reflect what the analyzer actually computed —
        // misleading for the project's primary documentation-analysis
        // use case. Render the Markdown family directly from the flat
        // metric map.
        write_markdown_metrics(out, metrics);
        return;
    }

    if language == Language::Sql {
        // SQL has its own relational/dataflow metric family (`sql.*`):
        // statements, query blocks, CTE graph, joins, predicates,
        // object-touch risk, and an SQL-flavored Halstead. Like
        // Markdown, the source-code families (cyclomatic/cognitive/…)
        // don't apply, so render the SQL groups directly.
        write_sql_metrics(out, metrics);
        return;
    }

    let families = MetricsFamilies::from_metrics(metrics);
    write_cyclomatic(out, &families.cyclomatic);
    write_cognitive(out, &families.cognitive);
    write_loc(out, &families.loc);
    write_halstead(out, &families.halstead);
    write_abc(out, &families.abc);
    write_nargs(out, &families.nargs);
    write_nom(out, &families.nom);
    write_nexits(out, &families.nexits);
    write_npa(out, &families.npa);
    write_npm(out, &families.npm);
    write_wmc(out, &families.wmc);
}

/// Render the `markdown.*` metric family as Markdown tables.
///
/// One table per documented group (LOC, LOC ratios, size, complexity,
/// Halstead, links, visuals, tables, maintainability, grounding,
/// ai_era, review). Field declaration order matches the publishing
/// order in `mehen_markdown::publish_markdown_metrics` so the rendered
/// section reads in the same shape as the §23 export schema.
fn write_markdown_metrics(out: &mut String, metrics: &MetricSet) {
    write_markdown_group(
        out,
        "LOC",
        &[
            ("dloc", "markdown.loc.dloc"),
            ("ploc", "markdown.loc.ploc"),
            ("cloc", "markdown.loc.cloc"),
            ("tloc", "markdown.loc.tloc"),
            ("mloc", "markdown.loc.mloc"),
            ("bloc", "markdown.loc.bloc"),
            ("aloc", "markdown.loc.aloc"),
        ],
        metrics,
    );
    write_markdown_group(
        out,
        "LOC ratios",
        &[
            (
                "artifact_line_ratio",
                "markdown.loc_ratios.artifact_line_ratio",
            ),
            ("code_line_ratio", "markdown.loc_ratios.code_line_ratio"),
            ("table_line_ratio", "markdown.loc_ratios.table_line_ratio"),
            ("math_line_ratio", "markdown.loc_ratios.math_line_ratio"),
            ("blank_line_ratio", "markdown.loc_ratios.blank_line_ratio"),
        ],
        metrics,
    );
    write_markdown_group(
        out,
        "Size",
        &[
            ("words", "markdown.size.words"),
            (
                "effective_content_units",
                "markdown.size.effective_content_units",
            ),
            ("sections", "markdown.size.sections"),
            ("headings", "markdown.size.headings"),
        ],
        metrics,
    );
    write_markdown_group(
        out,
        "Complexity",
        &[
            (
                "reading_path_complexity",
                "markdown.complexity.reading_path_complexity",
            ),
            (
                "reading_path_complexity_raw",
                "markdown.complexity.reading_path_complexity_raw",
            ),
            (
                "cognitive_complexity",
                "markdown.complexity.cognitive_complexity",
            ),
        ],
        metrics,
    );
    write_markdown_group(
        out,
        "Halstead",
        &[
            ("operators_distinct", "markdown.halstead.operators_distinct"),
            ("operators_total", "markdown.halstead.operators_total"),
            ("operands_distinct", "markdown.halstead.operands_distinct"),
            ("operands_total", "markdown.halstead.operands_total"),
            ("vocabulary", "markdown.halstead.vocabulary"),
            ("length", "markdown.halstead.length"),
            ("volume", "markdown.halstead.volume"),
            ("difficulty", "markdown.halstead.difficulty"),
            ("effort", "markdown.halstead.effort"),
            ("embedded_volume", "markdown.halstead.embedded_volume"),
            ("total_volume", "markdown.halstead.total_volume"),
        ],
        metrics,
    );
    write_markdown_group(
        out,
        "Links",
        &[
            ("total", "markdown.links.total"),
            ("broken", "markdown.links.broken"),
            ("link_debt_score", "markdown.links.link_debt_score"),
            (
                "information_scent_score",
                "markdown.links.information_scent_score",
            ),
            ("review_burden", "markdown.links.review_burden"),
        ],
        metrics,
    );
    write_markdown_group(
        out,
        "Visuals",
        &[
            ("images", "markdown.visuals.images"),
            ("diagrams", "markdown.visuals.diagrams"),
            (
                "diagram_parse_error_count",
                "markdown.visuals.diagram_parse_error_count",
            ),
            ("visual_net_effect", "markdown.visuals.visual_net_effect"),
        ],
        metrics,
    );
    write_markdown_group(
        out,
        "Tables",
        &[
            ("count", "markdown.tables.count"),
            ("max_cells", "markdown.tables.max_cells"),
            ("table_burden_score", "markdown.tables.table_burden_score"),
            ("hard_warnings", "markdown.tables.hard_warnings"),
        ],
        metrics,
    );
    write_markdown_group(
        out,
        "Maintainability",
        &[
            (
                "documentation_maintainability_index",
                "markdown.maintainability.documentation_maintainability_index",
            ),
            (
                "section_balance_score",
                "markdown.maintainability.section_balance_score",
            ),
            (
                "good_scaffold_score",
                "markdown.maintainability.good_scaffold_score",
            ),
            (
                "artifact_debt_score",
                "markdown.maintainability.artifact_debt_score",
            ),
        ],
        metrics,
    );
    write_markdown_group(
        out,
        "Grounding",
        &[
            (
                "repository_grounding_score",
                "markdown.grounding.repository_grounding_score",
            ),
            (
                "evidence_coverage_score",
                "markdown.grounding.evidence_coverage_score",
            ),
        ],
        metrics,
    );
    write_markdown_group(
        out,
        "AI era",
        &[(
            "filler_lazy_structure_risk",
            "markdown.ai_era.filler_lazy_structure_risk",
        )],
        metrics,
    );
    write_markdown_group(
        out,
        "Review",
        &[(
            "review_criticality_index",
            "markdown.review.review_criticality_index",
        )],
        metrics,
    );
}

/// Render the `sql.*` metric family as Markdown tables, one per documented
/// group. Mirrors the catalogue in `docs/metrics/sql/overview.mdx`.
fn write_sql_metrics(out: &mut String, metrics: &MetricSet) {
    write_markdown_group(
        out,
        "LOC",
        &[
            ("physical", "sql.loc.physical"),
            ("code", "sql.loc.code"),
            ("comment", "sql.loc.comment"),
            ("blank", "sql.loc.blank"),
            ("logical", "sql.loc.logical"),
            ("comment_density", "sql.loc.comment_density"),
        ],
        metrics,
    );
    write_markdown_group(
        out,
        "Statements",
        &[
            ("count", "sql.statement.count"),
            ("kind_distinct", "sql.statement.kind_distinct"),
            ("kind_entropy", "sql.statement.kind_entropy"),
            ("unparsed_count", "sql.statement.unparsed_count"),
        ],
        metrics,
    );
    write_markdown_group(
        out,
        "Query structure",
        &[
            ("query_block.count", "sql.query_block.count"),
            ("query_block.max_depth", "sql.query_block.max_depth"),
            ("cte.count", "sql.cte.count"),
            ("cte.max_dependency_depth", "sql.cte.max_dependency_depth"),
            ("cte.unused_count", "sql.cte.unused_count"),
            ("join.count", "sql.join.count"),
            ("subquery.correlated_count", "sql.subquery.correlated_count"),
        ],
        metrics,
    );
    write_markdown_group(
        out,
        "Expression complexity",
        &[
            (
                "predicate.boolean_operator_count",
                "sql.predicate.boolean_operator_count",
            ),
            ("case.count", "sql.case.count"),
            ("case.max_depth", "sql.case.max_depth"),
            ("window.function_count", "sql.window.function_count"),
            ("set_op.count", "sql.set_op.count"),
            ("expression.max_depth", "sql.expression.max_depth"),
        ],
        metrics,
    );
    write_markdown_group(
        out,
        "Object touch / risk",
        &[
            ("object.write_count", "sql.object.write_count"),
            ("ddl.drop_count", "sql.ddl.drop_count"),
            ("ddl.truncate_count", "sql.ddl.truncate_count"),
            (
                "dml.update_without_where_count",
                "sql.dml.update_without_where_count",
            ),
            (
                "dml.delete_without_where_count",
                "sql.dml.delete_without_where_count",
            ),
        ],
        metrics,
    );
    write_markdown_group(
        out,
        "Halstead",
        &[
            ("vocabulary", "sql.halstead.vocabulary"),
            ("length", "sql.halstead.length"),
            ("volume", "sql.halstead.volume"),
            ("difficulty", "sql.halstead.difficulty"),
            ("effort", "sql.halstead.effort"),
        ],
        metrics,
    );
    write_markdown_group(
        out,
        "Composite scores",
        &[
            ("structural_complexity", "sql.structural_complexity"),
            ("cognitive_complexity", "sql.cognitive_complexity"),
            ("change_risk_score", "sql.change_risk_score"),
            ("review_burden_index", "sql.review_burden_index"),
            ("maintainability_index", "sql.maintainability_index"),
            ("modularity_health", "sql.modularity_health"),
        ],
        metrics,
    );
    write_markdown_group(
        out,
        "Parser health",
        &[
            ("diagnostic_count", "sql.parser.diagnostic_count"),
            ("unparsable_ratio", "sql.parser.unparsable_ratio"),
            ("dialect.confidence", "sql.dialect.confidence"),
        ],
        metrics,
    );
}

fn write_markdown_group(
    out: &mut String,
    title: &str,
    columns: &[(&str, &str)],
    metrics: &MetricSet,
) {
    let _ = writeln!(out);
    let _ = writeln!(out, "### {title}");
    let _ = writeln!(out);
    let header: String = columns
        .iter()
        .map(|(label, _)| format!("| {label} "))
        .collect::<String>();
    let _ = writeln!(out, "{header}|");
    let separator: String = std::iter::repeat_n("|---:", columns.len()).collect();
    let _ = writeln!(out, "{separator}|");
    let row: String = columns
        .iter()
        .map(|(_, key)| format!("| {} ", fmt_metric(read_metric(metrics, key))))
        .collect();
    let _ = writeln!(out, "{row}|");
}

fn read_metric(metrics: &MetricSet, key: &str) -> f64 {
    metrics
        .get(&MetricKey::new(key))
        .map(|v| v.as_f64())
        .unwrap_or(0.0)
}

fn write_nested_spaces(out: &mut String, spaces: &[MetricSpace], depth: usize, language: Language) {
    if depth == 0 {
        // Print a section header only when at the top of the
        // recursion *and* there's something to show.
        if spaces.is_empty() {
            return;
        }
        let _ = writeln!(out);
        let _ = writeln!(out, "## Spaces");
    }
    for space in spaces {
        let header = "#".repeat(depth.saturating_add(3));
        let label = match (&space.kind, &space.name) {
            (SpaceKind::Unit, _) => "unit".to_string(),
            (kind, Some(name)) => format!("{} `{}`", space_kind_label(kind), name),
            (kind, None) => format!("{} (anonymous)", space_kind_label(kind)),
        };
        let _ = writeln!(out);
        let _ = writeln!(out, "{header} {label}");
        // The Markdown and SQL analyzers only publish flat unit-level
        // family metrics (`markdown.*` / `sql.*`); their nested spaces
        // (Markdown sections / embedded code, SQL per-statement spans)
        // carry no source-code roll-ups, so the Cyclomatic / Cognitive
        // / LOC tables would all be zero. Skip them in that case rather
        // than emit misleading numbers.
        if language != Language::Markdown && language != Language::Sql {
            let families = MetricsFamilies::from_metrics(&space.metrics);
            write_cyclomatic(out, &families.cyclomatic);
            write_cognitive(out, &families.cognitive);
            write_loc(out, &families.loc);
        }
        if !space.spaces.is_empty() {
            write_nested_spaces(out, &space.spaces, depth.saturating_add(1), language);
        }
    }
}

fn space_kind_label(kind: &SpaceKind) -> &'static str {
    match kind {
        SpaceKind::Unit => "unit",
        SpaceKind::Function => "function",
        SpaceKind::Closure => "closure",
        SpaceKind::Class => "class",
        SpaceKind::Interface => "interface",
        SpaceKind::Trait => "trait",
        SpaceKind::Impl => "impl",
        SpaceKind::Enum => "enum",
        SpaceKind::Custom(_) => "custom",
    }
}

// --- Per-family helpers --------------------------------------------

fn write_cyclomatic(out: &mut String, m: &Cyclomatic) {
    let _ = writeln!(out);
    let _ = writeln!(out, "### Cyclomatic");
    let _ = writeln!(out);
    let _ = writeln!(out, "| sum | average | min | max |");
    let _ = writeln!(out, "|---:|---:|---:|---:|");
    let _ = writeln!(
        out,
        "| {} | {} | {} | {} |",
        fmt_metric(m.sum),
        fmt_metric(m.average),
        fmt_metric(m.min),
        fmt_metric(m.max),
    );
}

fn write_cognitive(out: &mut String, m: &Cognitive) {
    let _ = writeln!(out);
    let _ = writeln!(out, "### Cognitive");
    let _ = writeln!(out);
    let _ = writeln!(out, "| sum | average | min | max |");
    let _ = writeln!(out, "|---:|---:|---:|---:|");
    let _ = writeln!(
        out,
        "| {} | {} | {} | {} |",
        fmt_metric(m.sum),
        fmt_metric(m.average),
        fmt_metric(m.min),
        fmt_metric(m.max),
    );
}

fn write_loc(out: &mut String, m: &Loc) {
    let _ = writeln!(out);
    let _ = writeln!(out, "### LOC");
    let _ = writeln!(out);
    let _ = writeln!(out, "| sloc | ploc | lloc | cloc | blank |");
    let _ = writeln!(out, "|---:|---:|---:|---:|---:|");
    let _ = writeln!(
        out,
        "| {} | {} | {} | {} | {} |",
        fmt_metric(m.sloc),
        fmt_metric(m.ploc),
        fmt_metric(m.lloc),
        fmt_metric(m.cloc),
        fmt_metric(m.blank),
    );
}

fn write_halstead(out: &mut String, m: &Halstead) {
    let _ = writeln!(out);
    let _ = writeln!(out, "### Halstead");
    let _ = writeln!(out);
    let _ = writeln!(out, "| n1 | N1 | n2 | N2 | volume | difficulty | effort |");
    let _ = writeln!(out, "|---:|---:|---:|---:|---:|---:|---:|");
    let _ = writeln!(
        out,
        "| {} | {} | {} | {} | {} | {} | {} |",
        fmt_metric(m.n1),
        fmt_metric(m.big_n1),
        fmt_metric(m.n2),
        fmt_metric(m.big_n2),
        fmt_metric(m.volume),
        fmt_metric(m.difficulty),
        fmt_metric(m.effort),
    );
}

fn write_abc(out: &mut String, m: &Abc) {
    let _ = writeln!(out);
    let _ = writeln!(out, "### ABC");
    let _ = writeln!(out);
    let _ = writeln!(out, "| assignments | branches | conditions | magnitude |");
    let _ = writeln!(out, "|---:|---:|---:|---:|");
    let _ = writeln!(
        out,
        "| {} | {} | {} | {} |",
        fmt_metric(m.assignments),
        fmt_metric(m.branches),
        fmt_metric(m.conditions),
        fmt_metric(m.magnitude),
    );
}

fn write_nargs(out: &mut String, m: &Nargs) {
    let _ = writeln!(out);
    let _ = writeln!(out, "### NArgs");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "| total_functions | total_closures | average | total |"
    );
    let _ = writeln!(out, "|---:|---:|---:|---:|");
    let _ = writeln!(
        out,
        "| {} | {} | {} | {} |",
        fmt_metric(m.total_functions),
        fmt_metric(m.total_closures),
        fmt_metric(m.average),
        fmt_metric(m.total),
    );
}

fn write_nom(out: &mut String, m: &Nom) {
    let _ = writeln!(out);
    let _ = writeln!(out, "### NOM");
    let _ = writeln!(out);
    let _ = writeln!(out, "| functions | closures | total |");
    let _ = writeln!(out, "|---:|---:|---:|");
    let _ = writeln!(
        out,
        "| {} | {} | {} |",
        fmt_metric(m.functions),
        fmt_metric(m.closures),
        fmt_metric(m.total),
    );
}

fn write_nexits(out: &mut String, m: &Nexits) {
    let _ = writeln!(out);
    let _ = writeln!(out, "### NExits");
    let _ = writeln!(out);
    let _ = writeln!(out, "| sum | average | min | max |");
    let _ = writeln!(out, "|---:|---:|---:|---:|");
    let _ = writeln!(
        out,
        "| {} | {} | {} | {} |",
        fmt_metric(m.sum),
        fmt_metric(m.average),
        fmt_metric(m.min),
        fmt_metric(m.max),
    );
}

fn write_npa(out: &mut String, m: &Npa) {
    let _ = writeln!(out);
    let _ = writeln!(out, "### NPA");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "| classes | interfaces | class_attributes | interface_attributes | total |"
    );
    let _ = writeln!(out, "|---:|---:|---:|---:|---:|");
    let _ = writeln!(
        out,
        "| {} | {} | {} | {} | {} |",
        fmt_metric(m.classes),
        fmt_metric(m.interfaces),
        fmt_metric(m.class_attributes),
        fmt_metric(m.interface_attributes),
        fmt_metric(m.total),
    );
}

fn write_npm(out: &mut String, m: &Npm) {
    let _ = writeln!(out);
    let _ = writeln!(out, "### NPM");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "| classes | interfaces | class_methods | interface_methods | total |"
    );
    let _ = writeln!(out, "|---:|---:|---:|---:|---:|");
    let _ = writeln!(
        out,
        "| {} | {} | {} | {} | {} |",
        fmt_metric(m.classes),
        fmt_metric(m.interfaces),
        fmt_metric(m.class_methods),
        fmt_metric(m.interface_methods),
        fmt_metric(m.total),
    );
}

fn write_wmc(out: &mut String, m: &Wmc) {
    let _ = writeln!(out);
    let _ = writeln!(out, "### WMC");
    let _ = writeln!(out);
    let _ = writeln!(out, "| classes | interfaces | total |");
    let _ = writeln!(out, "|---:|---:|---:|");
    let _ = writeln!(
        out,
        "| {} | {} | {} |",
        fmt_metric(m.classes),
        fmt_metric(m.interfaces),
        fmt_metric(m.total),
    );
}

/// Render an integer-valued metric as an integer when its
/// fractional component is zero (the common case for counts), and
/// as a 4-decimal float otherwise (for averages / ratios). NaN
/// surfaces as `nan` so a corrupt analyzer doesn't silently produce
/// `0` output.
fn fmt_metric(value: f64) -> String {
    if value.is_nan() {
        return "nan".to_string();
    }
    if value.fract() == 0.0 && value.is_finite() {
        return format!("{}", value as i64);
    }
    format!("{value:.4}")
}

fn escape_table_cell(s: &str) -> String {
    s.replace('|', r"\|").replace('\n', " ")
}

/// Phase 1 placeholder for the GitHub Markdown diff comment. Phase 4 ports
/// the existing pre-1.0 documentation diff renderer here. The output must
/// be byte-identical after stable timestamp/version redaction (parity
/// contract — rewrite plan §12.3.1).
pub fn render_diff_github_markdown(report: &DiffReport) -> String {
    format!(
        "<!-- mehen-schema:1 -->\n<!-- mehen-source -->\n\n_base: `{}`_  _head: `{}`_\n",
        report.base, report.head
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use mehen_core::{
        AnalysisBackend, Language, MetricKey, MetricSpace, ParseDiagnostic, SourceSpan, SpaceId,
        SpaceKind,
    };

    fn report_with_metrics(pairs: &[(&str, f64)]) -> MetricsReport {
        let mut root = MetricSpace::new(SpaceId(0), SpaceKind::Unit, SourceSpan::empty());
        for (k, v) in pairs {
            root.metrics.insert(MetricKey::new(*k), *v);
        }
        MetricsReport {
            schema_version: "1.0".to_string(),
            tool: "mehen".to_string(),
            path: "foo.py".into(),
            language: Language::Python,
            analysis_backend: AnalysisBackend::PythonRuff,
            diagnostics: Vec::new(),
            root,
        }
    }

    #[test]
    fn renders_metric_values_not_just_metadata() {
        // Regression: prior implementation emitted only path / language /
        // backend, dropping every metric value the analyzer published.
        // Anyone calling `mehen metrics ... --format markdown` would see
        // numbers vanish from CI artefacts and docs builds.
        let report = report_with_metrics(&[
            ("cyclomatic.sum", 7.0),
            ("loc.sloc", 42.0),
            ("loc.lloc", 30.0),
            ("halstead.volume", 123.5),
        ]);
        let md = render_metrics_markdown(&report);

        // File metadata is still there.
        assert!(md.contains("# foo.py"));
        assert!(md.contains("- language: `python`"));

        // The actual metric numbers must surface.
        assert!(
            md.contains("## Metrics"),
            "missing Metrics section in output:\n{md}"
        );
        assert!(
            md.contains("### Cyclomatic"),
            "missing Cyclomatic table in output:\n{md}"
        );
        assert!(md.contains("| 7 |"), "cyclomatic.sum 7 not rendered:\n{md}");
        assert!(md.contains("### LOC"), "missing LOC table");
        assert!(
            md.contains("| 42 | 0 | 30 | 0 | 0 |"),
            "LOC row not rendered:\n{md}"
        );
        assert!(
            md.contains("### Halstead"),
            "missing Halstead table in output:\n{md}"
        );
        assert!(
            md.contains("123.5000"),
            "halstead.volume 123.5 not rendered:\n{md}"
        );
    }

    #[test]
    fn emits_diagnostics_section_when_present() {
        let mut report = report_with_metrics(&[]);
        report.diagnostics.push(ParseDiagnostic::error(
            "python.parse_error",
            "unexpected EOF while parsing",
        ));
        report.diagnostics.push(ParseDiagnostic::warning(
            "python.style",
            "long line | with pipe",
        ));
        let md = render_metrics_markdown(&report);
        assert!(md.contains("## Diagnostics"));
        assert!(md.contains("| error | `python.parse_error` | unexpected EOF while parsing |"));
        // Pipe characters in messages must be escaped so they don't
        // break the table layout.
        assert!(md.contains(r"long line \| with pipe"));
    }

    #[test]
    fn skips_diagnostics_section_when_empty() {
        let report = report_with_metrics(&[("cyclomatic.sum", 1.0)]);
        let md = render_metrics_markdown(&report);
        assert!(!md.contains("## Diagnostics"));
    }

    #[test]
    fn renders_markdown_family_when_language_is_markdown() {
        // Regression: previously `write_unit_metrics` always pivoted
        // through `MetricsFamilies`, which only reads source-code
        // metric keys (`cyclomatic.*`, `loc.sloc`, `halstead.volume`,
        // etc.). For `mehen metrics README.md --format markdown` the
        // analyzer publishes `markdown.*` keys, so the pivot would
        // emit all-zero source-code tables — misleading for the
        // project's primary documentation-analysis use case.
        let mut root = MetricSpace::new(SpaceId(0), SpaceKind::Unit, SourceSpan::empty());
        for (k, v) in &[
            ("markdown.size.words", 1234.0),
            ("markdown.size.headings", 8.0),
            ("markdown.complexity.cognitive_complexity", 17.5),
            (
                "markdown.maintainability.documentation_maintainability_index",
                88.25,
            ),
            ("markdown.links.broken", 2.0),
            ("markdown.halstead.volume", 999.0),
        ] {
            root.metrics.insert(MetricKey::new(*k), *v);
        }
        let report = MetricsReport {
            schema_version: "1.0".to_string(),
            tool: "mehen".to_string(),
            path: "README.md".into(),
            language: Language::Markdown,
            analysis_backend: AnalysisBackend::PulldownCmark,
            diagnostics: Vec::new(),
            root,
        };
        let md = render_metrics_markdown(&report);

        // Markdown family sections must surface.
        assert!(md.contains("### LOC"), "missing LOC section: {md}");
        assert!(md.contains("### Size"), "missing Size section: {md}");
        assert!(
            md.contains("### Complexity"),
            "missing Complexity section: {md}"
        );
        assert!(md.contains("### Halstead"), "missing Halstead section");
        assert!(md.contains("### Links"), "missing Links section");
        assert!(
            md.contains("### Maintainability"),
            "missing Maintainability section"
        );

        // The actual published values must render — not as zeros.
        assert!(md.contains("| 1234 |"), "size.words 1234 missing: {md}");
        assert!(md.contains("| 8 |"), "size.headings 8 missing: {md}");
        assert!(
            md.contains("17.5000"),
            "cognitive_complexity 17.5 missing: {md}"
        );
        assert!(
            md.contains("88.2500"),
            "documentation_maintainability_index 88.25 missing: {md}"
        );
        assert!(md.contains("| 2 |"), "links.broken 2 missing: {md}");
        assert!(md.contains("| 999 |"), "halstead.volume 999 missing: {md}");

        // Source-code family tables must NOT appear for a Markdown
        // report — they would all be zero and add noise.
        assert!(
            !md.contains("### Cyclomatic"),
            "Cyclomatic table should be skipped for Markdown: {md}"
        );
        assert!(
            !md.contains("### ABC"),
            "ABC table should be skipped for Markdown: {md}"
        );
        assert!(
            !md.contains("### NArgs"),
            "NArgs table should be skipped for Markdown: {md}"
        );
        assert!(
            !md.contains("### NPA"),
            "NPA table should be skipped for Markdown: {md}"
        );
    }

    #[test]
    fn includes_nested_spaces_when_present() {
        let mut root = MetricSpace::new(SpaceId(0), SpaceKind::Unit, SourceSpan::empty());
        root.metrics.insert(MetricKey::new("cyclomatic.sum"), 5.0);
        let mut child = MetricSpace::new(SpaceId(1), SpaceKind::Function, SourceSpan::empty());
        child.name = Some("foo".to_string());
        child.metrics.insert(MetricKey::new("cyclomatic.sum"), 2.0);
        root.spaces.push(child);
        let report = MetricsReport {
            schema_version: "1.0".to_string(),
            tool: "mehen".to_string(),
            path: "foo.py".into(),
            language: Language::Python,
            analysis_backend: AnalysisBackend::PythonRuff,
            diagnostics: Vec::new(),
            root,
        };
        let md = render_metrics_markdown(&report);
        assert!(md.contains("## Spaces"));
        assert!(md.contains("function `foo`"));
        // The nested space's cyclomatic.sum should appear in its
        // own table.
        let space_section = md.split("## Spaces").nth(1).expect("Spaces section");
        assert!(space_section.contains("| 2 |"));
    }
}
