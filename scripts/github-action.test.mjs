import assert from "node:assert/strict";
import test from "node:test";

import {
  DEFAULT_TEST_EXCLUDES,
  alignFileMetrics,
  canonicalMetricName,
  collectThresholdViolations,
  diffJsonHasDocs,
  extractMarkdownDocsSection,
  formatMetricCell,
  inferPolarity,
  isGateFailureReport,
  isNotApplicable,
  parseGateViolations,
  parseList,
  parseThresholds,
  parseVersionOutput,
  renderFooter,
  unionMetricColumns,
} from "./github-action.mjs";

test("canonicalMetricName preserves case-distinct halstead count keys", () => {
  // `n1`/`N1` (and `n2`/`N2`) are distinct published measurements —
  // distinct vs total operator/operand counts; lowercasing would
  // gate the wrong one.
  assert.equal(canonicalMetricName("halstead.N1"), "halstead.N1");
  assert.equal(canonicalMetricName("halstead.n1"), "halstead.n1");
  assert.equal(canonicalMetricName("halstead.N2"), "halstead.N2");
  assert.equal(canonicalMetricName("halstead.n2"), "halstead.n2");
  // Everything else keeps the legacy case-insensitive aliasing.
  assert.equal(canonicalMetricName("Cognitive"), "cognitive");
  assert.equal(canonicalMetricName("loc"), "loc.lloc");
  assert.equal(canonicalMetricName("nom"), "nom.functions");
});

test("parseGateViolations extracts the embedded gate breaches", () => {
  const payload =
    '{"source_code": [], "threshold_violations": [{"path": "a.py", "metric": "cognitive", "value": 23, "limit": 15, "polarity": "higher_is_worse", "source_table": "languages.py.thresholds"}]}';
  const violations = parseGateViolations(payload);
  assert.equal(violations.length, 1);
  assert.equal(violations[0].metric, "cognitive");
  assert.equal(violations[0].source_table, "languages.py.thresholds");
});

test("parseGateViolations is empty for passing runs and older CLIs", () => {
  assert.deepEqual(parseGateViolations('{"source_code": []}'), []);
  assert.deepEqual(parseGateViolations("[]"), []);
  assert.deepEqual(parseGateViolations("not json"), []);
  assert.deepEqual(parseGateViolations(undefined), []);
});

test("isGateFailureReport requires the explicit threshold_violations signal", () => {
  assert.equal(
    isGateFailureReport(
      '{"source_code": [], "threshold_violations": [{"path": "a.py", "metric": "cognitive"}]}',
    ),
    true,
  );
  assert.equal(
    isGateFailureReport(
      '{"source_code": [{"path": "a.py"}], "markdown": [], "threshold_violations": [{}]}',
    ),
    true,
  );
});

test("isGateFailureReport rejects reports without a fired gate", () => {
  // An analysis failure also exits 1 with well-formed JSON — without
  // the threshold_violations key it must keep failing fast.
  assert.equal(isGateFailureReport('{"source_code": [], "markdown": []}'), false);
  assert.equal(isGateFailureReport('{"source_code": [{"path": "a.py"}]}'), false);
  assert.equal(
    isGateFailureReport('{"source_code": [], "threshold_violations": []}'),
    false,
  );
});

test("isGateFailureReport rejects partial or non-JSON output", () => {
  // A setup/IO failure leaves stdout empty or truncated — that must
  // keep failing fast instead of being treated as a quality gate.
  assert.equal(isGateFailureReport(""), false);
  assert.equal(isGateFailureReport("error: not json"), false);
  assert.equal(isGateFailureReport('{"source_code": '), false);
  assert.equal(isGateFailureReport('{"markdown": []}'), false);
  assert.equal(isGateFailureReport(undefined), false);
});

test("parseList uses explicit separators only", () => {
  assert.deepEqual(parseList("src"), ["src"]);
  assert.deepEqual(parseList("apps/web src"), ["apps/web src"]);
  assert.deepEqual(parseList("apps/web\ncrates/api,tools;fixtures/data"), [
    "apps/web",
    "crates/api",
    "tools",
    "fixtures/data",
  ]);
});

test("parseList preserves paths and thresholds containing spaces", () => {
  assert.deepEqual(parseList("my folder"), ["my folder"]);
  assert.deepEqual(parseList("cyclomatic = 5"), ["cyclomatic = 5"]);
});

test("DEFAULT_TEST_EXCLUDES covers common test filename patterns", () => {
  for (const pattern of [
    "**/*_test.go",
    "**/__tests__/**",
    "**/*.test.ts",
    "**/*.spec.ts",
    "**/tests/**",
  ]) {
    assert.ok(
      DEFAULT_TEST_EXCLUDES.includes(pattern),
      `expected DEFAULT_TEST_EXCLUDES to include ${pattern}`,
    );
  }
});

test("parseThresholds accepts whitespace around operators", () => {
  const thresholds = parseThresholds("cyclomatic = 5\ncognitive: 4,loc.lloc <= 120");

  assert.equal(thresholds.get("cyclomatic"), 5);
  assert.equal(thresholds.get("cognitive"), 4);
  assert.equal(thresholds.get("loc.lloc"), 120);
});

test("diffJsonHasDocs detects the documentation section", () => {
  // The docs rerun (a second full `mehen diff`) must only happen when
  // the JSON payload actually carries a markdown section.
  assert.equal(diffJsonHasDocs(JSON.stringify({ source_code: [] })), false);
  assert.equal(
    diffJsonHasDocs(JSON.stringify({ source_code: [], markdown: [] })),
    false,
  );
  assert.equal(
    diffJsonHasDocs(
      JSON.stringify({ source_code: [], markdown: [{ path: "README.md" }] }),
    ),
    true,
  );
  assert.equal(diffJsonHasDocs("not json"), false);
  assert.equal(diffJsonHasDocs(undefined), false);
});

test("isNotApplicable detects explicit flag and missing values", () => {
  assert.equal(isNotApplicable({ not_applicable: true, current: 0, baseline: 0 }), true);
  assert.equal(isNotApplicable({ current: null, baseline: null }), true);
  assert.equal(isNotApplicable({ current: undefined, baseline: undefined }), true);
  assert.equal(isNotApplicable({ current: 0, baseline: 0 }), false);
  assert.equal(isNotApplicable({ current: 3, baseline: null }), false);
});

test("formatMetricCell renders em dash for non-applicable metrics", () => {
  assert.equal(formatMetricCell({ not_applicable: true }, "main"), "—");
  assert.equal(formatMetricCell({ current: null, baseline: null }, "main"), "—");
});

test("formatMetricCell still renders normal values", () => {
  const metric = {
    name: "cyclomatic",
    label: "Cyclomatic",
    current: 5,
    baseline: 3,
    delta: 2,
    polarity: "lower-is-better",
  };
  assert.ok(formatMetricCell(metric, "main").startsWith("5 (main: 3)"));
});

test("formatMetricCell honors unavailable sides without claiming a trend", () => {
  // A side flagged unavailable carries a numeric 0.0 placeholder that
  // must not render as a real zero (or a green "improvement").
  const base = {
    name: "history.hotspot",
    label: "Hotspot",
    current: 0,
    baseline: 12,
    delta: 0,
    polarity: "lower-is-better",
  };
  assert.equal(
    formatMetricCell({ ...base, current_unavailable: true }, "main"),
    "n/a (main: 12)",
  );
  assert.equal(
    formatMetricCell({ ...base, baseline_unavailable: true }, "main"),
    "0 (main: n/a)",
  );
  assert.equal(
    formatMetricCell(
      { ...base, current_unavailable: true, baseline_unavailable: true },
      "main",
    ),
    "n/a",
  );
  assert.equal(
    formatMetricCell({ ...base, current_unavailable: true, is_new: true }, "main"),
    "n/a \u{1F195}",
  );
  assert.equal(
    formatMetricCell(
      { ...base, baseline_unavailable: true, is_deleted: true },
      "main",
    ),
    "0 (was: n/a)",
  );
});

test("collectThresholdViolations skips unavailable placeholder deltas", () => {
  const diffs = [
    {
      path: "broken.py",
      metrics: [
        {
          name: "cognitive",
          label: "Cognitive",
          current: 0,
          baseline: 12,
          delta: -12,
          current_unavailable: true,
          polarity: "lower-is-better",
        },
      ],
    },
  ];
  const thresholds = new Map([["cognitive", 1]]);
  assert.deepEqual(collectThresholdViolations(diffs, thresholds), []);
});

test("unionMetricColumns includes metrics only present in later files", () => {
  const diffs = [
    {
      path: "foo.go",
      metrics: [{ name: "cyclomatic", label: "Cyclomatic" }],
    },
    {
      path: "bar.py",
      metrics: [
        { name: "cyclomatic", label: "Cyclomatic" },
        { name: "wmc", label: "WMC" },
      ],
    },
  ];
  const columns = unionMetricColumns(diffs);
  assert.deepEqual(
    columns.map((c) => c.name),
    ["cyclomatic", "wmc"],
  );
});

test("alignFileMetrics fills missing metrics with a non-applicable placeholder", () => {
  const header = [
    { name: "cyclomatic", label: "Cyclomatic" },
    { name: "wmc", label: "WMC", polarity: "lower-is-better" },
  ];
  const fileMetrics = [
    {
      name: "cyclomatic",
      label: "Cyclomatic",
      current: 5,
      baseline: 3,
      delta: 2,
      polarity: "lower-is-better",
    },
  ];
  const aligned = alignFileMetrics(fileMetrics, header);
  assert.equal(aligned.length, 2);
  assert.equal(aligned[0].current, 5);
  assert.equal(isNotApplicable(aligned[1]), true);
  assert.equal(aligned[1].name, "wmc");
});

test("alignFileMetrics preserves existing metrics when present", () => {
  const header = [{ name: "cyclomatic", label: "Cyclomatic" }];
  const source = {
    name: "cyclomatic",
    label: "Cyclomatic",
    current: 1,
    baseline: 1,
    delta: 0,
  };
  const aligned = alignFileMetrics([source], header);
  assert.equal(aligned.length, 1);
  assert.equal(aligned[0], source);
});

test("inferPolarity treats MI variants as higher-is-better", () => {
  assert.equal(inferPolarity("mi.original"), "higher-is-better");
  assert.equal(inferPolarity("mi.sei"), "higher-is-better");
  assert.equal(inferPolarity("mi.visual_studio"), "higher-is-better");
  assert.equal(inferPolarity("cyclomatic"), "lower-is-better");
});

test("parseVersionOutput extracts version from --version --json payload", () => {
  assert.equal(
    parseVersionOutput('{"name":"mehen","version":"0.4.3"}'),
    "0.4.3",
  );
  assert.equal(
    parseVersionOutput('  {"name":"mehen","version":"1.2.3-beta.1"}  \n'),
    "1.2.3-beta.1",
  );
});

test("parseVersionOutput returns empty string for unparsable input", () => {
  assert.equal(parseVersionOutput(""), "");
  assert.equal(parseVersionOutput("mehen 0.4.3"), "");
  assert.equal(parseVersionOutput("{}"), "");
});

test("renderFooter includes version when provided", () => {
  const footer = renderFooter("0.4.3");
  assert.ok(footer.includes("mehen"));
  assert.ok(footer.includes("v0.4.3"));
  assert.ok(footer.includes("code quality watcher"));
});

test("renderFooter omits version suffix when missing", () => {
  const footer = renderFooter("");
  assert.ok(footer.includes("mehen"));
  assert.ok(!footer.includes(" v "));
  assert.ok(!/v\d/.test(footer));
});

test("extractMarkdownDocsSection returns null for empty or whitespace-only input", () => {
  assert.equal(extractMarkdownDocsSection(""), null);
  assert.equal(extractMarkdownDocsSection("   \n\t  "), null);
  assert.equal(extractMarkdownDocsSection(null), null);
  assert.equal(extractMarkdownDocsSection(undefined), null);
});

test("extractMarkdownDocsSection returns null when the anchor is missing", () => {
  const stdout = [
    "## [Mehen] Summary",
    "",
    "| File | Cyclomatic |",
    "|---|---:|",
    "| src/main.rs | 3 (main: 2) 🔴 |",
  ].join("\n");
  assert.equal(extractMarkdownDocsSection(stdout), null);
});

test("extractMarkdownDocsSection returns null when the anchor is present but the section is empty", () => {
  assert.equal(extractMarkdownDocsSection("<!-- mehen-docs -->"), null);
  assert.equal(extractMarkdownDocsSection("prelude\n<!-- mehen-docs -->\n\n  "), null);
});

test("extractMarkdownDocsSection slices from the anchor to end-of-output and trims", () => {
  const section = [
    "<!-- mehen-docs -->",
    "## Documentation Metrics (this PR vs `main`)",
    "",
    "| File | DMI |",
    "|---|---:|",
    "| README.md | 74 (main: 71) 🟢 |",
  ].join("\n");
  const stdout = `## [Mehen] Summary\n\n| File |\n|---|\n\n${section}\n\n`;
  const extracted = extractMarkdownDocsSection(stdout);
  assert.equal(extracted, section);
});

test("extractMarkdownDocsSection preserves later anchors as literal text", () => {
  // Defensive: if the CLI ever emits the anchor twice (e.g. inside a
  // fenced example), indexOf finds the first one and we keep everything
  // after it — the second anchor stays embedded rather than re-splitting.
  const stdout = [
    "<!-- mehen-docs -->",
    "## Documentation Metrics",
    "",
    "```markdown",
    "<!-- mehen-docs -->",
    "```",
  ].join("\n");
  const extracted = extractMarkdownDocsSection(stdout);
  assert.ok(extracted?.startsWith("<!-- mehen-docs -->"));
  assert.ok(extracted.includes("```markdown"));
});

test("collectThresholdViolations skips non-applicable metrics", () => {
  const thresholds = parseThresholds("wmc=5");
  const diffs = [
    {
      path: "pkg/foo.go",
      metrics: [
        {
          name: "wmc",
          label: "WMC",
          not_applicable: true,
          current: null,
          baseline: null,
          delta: 0,
          polarity: "lower-is-better",
        },
      ],
    },
  ];
  const violations = collectThresholdViolations(diffs, thresholds);
  assert.deepEqual(violations, []);
});
