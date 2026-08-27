import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import zlib from "node:zlib";

import {
  DEFAULT_TEST_EXCLUDES,
  alignFileMetrics,
  buildDiffArgs,
  canonicalMetricName,
  codecovToLcov,
  collectThresholdViolations,
  countBlockingAnalysisDiagnostics,
  diffJsonHasDocs,
  extractMarkdownDocsSection,
  extractZip,
  formatMetricCell,
  inferPolarity,
  isBaseCoverageFailure,
  isGateFailureReport,
  isNotApplicable,
  listFilesRecursively,
  parseAnalysisErrors,
  parseGateViolations,
  parseList,
  parseThresholds,
  parseVersionOutput,
  pickBaseArtifact,
  readGithubContext,
  renderFooter,
  renderMarkdown,
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

test("parseAnalysisErrors extracts structured per-side diagnostics", () => {
  const errors = parseAnalysisErrors(
    JSON.stringify({
      source_code: [],
      analysis_errors: [
        {
          path: "internal/config/rules.go",
          side: "head",
          diagnostics: [
            {
              severity: "error",
              code: "go.syntax_error",
              message: "tree-sitter error node at line 347",
              span: null,
            },
          ],
        },
      ],
    }),
  );
  assert.equal(errors.length, 1);
  assert.equal(errors[0].path, "internal/config/rules.go");
});

test("parseAnalysisErrors is empty for older or malformed output", () => {
  assert.deepEqual(parseAnalysisErrors('{"source_code": []}'), []);
  assert.deepEqual(parseAnalysisErrors("not json"), []);
  assert.deepEqual(parseAnalysisErrors(undefined), []);
});

test("blocking analysis diagnostic count deduplicates parser recovery noise", () => {
  const duplicate = {
    severity: "error",
    code: "go.syntax_error",
    message: "tree-sitter error node at line 347",
    span: null,
  };
  const records = [
    {
      path: "internal/config/rules.go",
      side: "head",
      diagnostics: [duplicate, { ...duplicate }],
    },
    {
      path: "README.md",
      side: "head",
      diagnostics: [
        {
          severity: "warning",
          code: "markdown.reference",
          message: "reference could not be resolved",
        },
      ],
    },
  ];
  assert.equal(countBlockingAnalysisDiagnostics(records), 1);
});

test("renderMarkdown reports analysis diagnostics without hiding other results", () => {
  const markdown = renderMarkdown(
    [],
    {
      eventName: "pull_request",
      repository: "wharflab/tally",
      sha: "event-head-sha",
      baseSha: "event-base-sha",
      headRevision: "analyzed-head-sha",
      baseRevision: "analyzed-base-sha",
      baseLabel: "main",
    },
    new Map(),
    [],
    "1.11.0",
    [],
    null,
    [
      {
        path: "internal/config/rules.go",
        side: "head",
        diagnostics: [
          {
            severity: "error",
            code: "go.syntax_error",
            message: "tree-sitter error node at line 347",
          },
        ],
      },
      {
        path: "internal/config/old-rules.go",
        side: "base",
        diagnostics: [
          {
            severity: "error",
            code: "go.syntax_error",
            message: "tree-sitter error node at line 12",
          },
        ],
      },
    ],
  );
  assert.ok(markdown.includes("### Analysis diagnostics"));
  assert.ok(markdown.includes("go.syntax_error"));
  assert.ok(markdown.includes("internal/config/rules.go"));
  assert.ok(markdown.includes("analyzed-head-sha"));
  assert.ok(markdown.includes("analyzed-base-sha"));
  assert.ok(!markdown.includes("event-head-sha"));
  assert.ok(!markdown.includes("event-base-sha"));
  assert.ok(markdown.includes("No metric changes detected."));
});

test("readGithubContext resolves explicit analysis refs separately from event SHAs", () => {
  const repo = fs.mkdtempSync(path.join(os.tmpdir(), "mehen-action-context-"));
  const gitEnv = {
    ...process.env,
    GIT_AUTHOR_NAME: "Mehen Test",
    GIT_AUTHOR_EMAIL: "test@mehen.invalid",
    GIT_COMMITTER_NAME: "Mehen Test",
    GIT_COMMITTER_EMAIL: "test@mehen.invalid",
  };
  const git = (...args) =>
    execFileSync("git", args, { cwd: repo, env: gitEnv, encoding: "utf8" }).trim();
  git("init", "-q", "-b", "main");
  fs.writeFileSync(path.join(repo, "sample.txt"), "base\n", "utf8");
  git("add", "sample.txt");
  git("commit", "-q", "-m", "base");
  const base = git("rev-parse", "HEAD");
  fs.writeFileSync(path.join(repo, "sample.txt"), "head\n", "utf8");
  git("commit", "-q", "-am", "head");
  const head = git("rev-parse", "HEAD");

  const eventPath = path.join(repo, "event.json");
  fs.writeFileSync(
    eventPath,
    JSON.stringify({
      number: 261,
      pull_request: {
        number: 261,
        base: { ref: "main", sha: "event-base-sha" },
        head: { sha: "event-head-sha" },
      },
    }),
    "utf8",
  );

  const names = [
    "GHA_MEHEN_FROM",
    "GHA_MEHEN_TO",
    "GITHUB_EVENT_PATH",
    "GITHUB_EVENT_NAME",
    "GITHUB_REPOSITORY",
  ];
  const saved = new Map(names.map((name) => [name, process.env[name]]));
  try {
    process.env.GHA_MEHEN_FROM = "HEAD~1";
    process.env.GHA_MEHEN_TO = "HEAD";
    process.env.GITHUB_EVENT_PATH = eventPath;
    process.env.GITHUB_EVENT_NAME = "pull_request";
    process.env.GITHUB_REPOSITORY = "ophi-dev/mehen";

    const context = readGithubContext(repo);
    assert.equal(context.baseSha, "event-base-sha");
    assert.equal(context.sha, "event-head-sha");
    assert.equal(context.baseRevision, base);
    assert.equal(context.headRevision, head);
    assert.equal(context.baseLabel, "HEAD~1");
  } finally {
    for (const [name, value] of saved) {
      if (value === undefined) {
        delete process.env[name];
      } else {
        process.env[name] = value;
      }
    }
  }
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
  // Analysis diagnostics use their own advisory array and are not a
  // repository-threshold gate signal.
  assert.equal(isGateFailureReport('{"source_code": [], "markdown": []}'), false);
  assert.equal(isGateFailureReport('{"source_code": [{"path": "a.py"}]}'), false);
  assert.equal(
    isGateFailureReport(
      '{"source_code": [], "analysis_errors": [{"path": "a.py"}]}',
    ),
    false,
  );
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


// ── Base coverage retrieval (issue #248) ─────────────────────────────

test("codecovToLcov maps hit, miss, and partial statuses to DA records", () => {
  const lcov = codecovToLcov({
    totals: { coverage: 66.67 },
    files: [
      {
        name: "src/lib.rs",
        totals: { lines: 3 },
        // 0 = hit, 1 = miss, 2 = partial (partial executed → hit).
        line_coverage: [
          [1, 0],
          [2, 1],
          [3, 2],
        ],
      },
    ],
  });
  assert.equal(lcov, "SF:src/lib.rs\nDA:1,1\nDA:2,0\nDA:3,1\nend_of_record\n");
});

test("codecovToLcov never fabricates branch records", () => {
  const lcov = codecovToLcov({
    files: [
      {
        name: "a.py",
        line_coverage: [
          [1, 2],
          [2, 2],
        ],
      },
    ],
  });
  // Partials come from branch data upstream, but codecov's merged view
  // has no original arms — inventing BRDA records would poison
  // coverage.branch gates.
  assert.ok(!lcov.includes("BRDA"));
  assert.equal(lcov, "SF:a.py\nDA:1,1\nDA:2,1\nend_of_record\n");
});

test("codecovToLcov skips malformed entries and unknown statuses", () => {
  const lcov = codecovToLcov({
    files: [
      {
        name: "b.go",
        line_coverage: [
          [1, 0],
          [0, 0], // non-positive line
          [-3, 1], // negative line
          [2.5, 0], // fractional line
          [4], // too short
          "junk", // not an array
          [5, "1/2"], // unknown status encoding → skipped, not guessed
          [6, 3], // unknown numeric status
          [7, 1],
        ],
      },
    ],
  });
  assert.equal(lcov, "SF:b.go\nDA:1,1\nDA:7,0\nend_of_record\n");
});

test("codecovToLcov returns null when nothing usable remains", () => {
  assert.equal(codecovToLcov(null), null);
  assert.equal(codecovToLcov({}), null);
  assert.equal(codecovToLcov({ files: [] }), null);
  // A file without line data contributes nothing; an empty LCOV would
  // fail mehen's format sniff, so the ladder must degrade to absent.
  assert.equal(
    codecovToLcov({ files: [{ name: "a.rs", line_coverage: [] }] }),
    null,
  );
  assert.equal(
    codecovToLcov({ files: [{ name: "", line_coverage: [[1, 0]] }] }),
    null,
  );
  assert.equal(
    codecovToLcov({ files: [{ name: "c.ts", line_coverage: [[1, 9]] }] }),
    null,
  );
});

test("codecovToLcov emits one record block per usable file", () => {
  const lcov = codecovToLcov({
    files: [
      { name: "a.rs", line_coverage: [[1, 0]] },
      { name: "skipped.rs", line_coverage: [] },
      { name: "b.rs", line_coverage: [[9, 1]] },
    ],
  });
  assert.equal(
    lcov,
    "SF:a.rs\nDA:1,1\nend_of_record\nSF:b.rs\nDA:9,0\nend_of_record\n",
  );
});

test("listFilesRecursively returns [] for absent or empty inputs", () => {
  assert.deepEqual(listFilesRecursively(""), []);
  assert.deepEqual(listFilesRecursively(undefined), []);
  assert.deepEqual(
    listFilesRecursively("/nonexistent/mehen-test-dir"),
    [],
  );
});


test("every top-level const is declared before the entrypoint block", () => {
  // `main()` is invoked from the `isEntrypoint()` block during module
  // evaluation, so its synchronous call graph runs before any
  // statement below that block has executed. Function declarations
  // hoist; `const` bindings do not — a top-level const declared after
  // the block is a temporal-dead-zone crash waiting for the first
  // synchronous path that reads it. Seen live in CI as "Cannot access
  // 'CODECOV_PENDING_RETRIES' before initialization"; importing the
  // module (as these tests do) can never reproduce it, hence this
  // source-order invariant.
  const source = fs.readFileSync(
    new URL("./github-action.mjs", import.meta.url),
    "utf8",
  );
  const entry = source.indexOf("if (isEntrypoint())");
  assert.ok(entry > 0, "entrypoint block must be present");
  const offender = source.slice(entry).match(/^const\s+\S+/m);
  assert.equal(
    offender,
    null,
    `top-level const declared after the entrypoint block: '${offender?.[0]}'`,
  );
});


test("buildDiffArgs pins coverage off when every configured report is missing", () => {
  const saved = process.env.GHA_MEHEN_COVERAGE_FILES;
  try {
    // All configured files missing: without the pin, a --base-coverage
    // argument would flip mehen's lazy trigger into head-side
    // auto-discovery, substituting stale working-tree artifacts for
    // the reports the caller explicitly configured.
    process.env.GHA_MEHEN_COVERAGE_FILES =
      "/nonexistent/mehen-a.info,/nonexistent/mehen-b.info";
    const args = buildDiffArgs(["--base-coverage=/tmp/base.lcov"]);
    assert.ok(args.includes("--coverage=off"), args.join(" "));
    assert.ok(
      !args.some((a) => a.startsWith("--coverage=/")),
      "missing files must not be passed through",
    );
    assert.ok(args.includes("--base-coverage=/tmp/base.lcov"));

    // No coverage configured at all: no pin — lazy semantics stay.
    process.env.GHA_MEHEN_COVERAGE_FILES = "";
    assert.ok(!buildDiffArgs().includes("--coverage=off"));
  } finally {
    if (saved === undefined) {
      delete process.env.GHA_MEHEN_COVERAGE_FILES;
    } else {
      process.env.GHA_MEHEN_COVERAGE_FILES = saved;
    }
  }
});

test("isBaseCoverageFailure matches only stderr naming a base report path", () => {
  const baseArgs = ["--base-coverage=/tmp/mehen-base-coverage/lcov.info"];
  const failure = (stderr) => ({ stderr });
  // mehen's setup error names the offending report.
  assert.equal(
    isBaseCoverageFailure(
      failure(
        "[ERROR] failed to parse coverage report `/tmp/mehen-base-coverage/lcov.info`: truncated record",
      ),
      baseArgs,
    ),
    true,
  );
  // A corrupt *head* report is the caller's own artifact — no retry.
  assert.equal(
    isBaseCoverageFailure(
      failure("[ERROR] failed to parse coverage report `coverage/lcov.info`"),
      baseArgs,
    ),
    false,
  );
  // Unrelated failures, missing stderr, or no base args: never retry.
  assert.equal(
    isBaseCoverageFailure(failure("[ERROR] git: object not found"), baseArgs),
    false,
  );
  assert.equal(isBaseCoverageFailure(new Error("spawn failed"), baseArgs), false);
  assert.equal(
    isBaseCoverageFailure(failure("failed to parse coverage report"), []),
    false,
  );
});


// ── Workflow-artifact base source (issue #254, rung 2) ───────────────

/**
 * Build a standard ZIP archive in memory — local headers, central
 * directory, end-of-central-directory — so extractZip is tested
 * against real archive bytes without a binary fixture. Entries:
 * `{ name, content, method }` with method 0 (stored) or 8 (deflate),
 * matching what GitHub serves for workflow artifacts.
 */
function buildZip(entries) {
  const localParts = [];
  const centralParts = [];
  let offset = 0;
  for (const entry of entries) {
    const nameBytes = Buffer.from(entry.name, "utf8");
    const raw = Buffer.from(entry.content ?? "", "utf8");
    const method = entry.method ?? 8;
    const data = method === 8 ? zlib.deflateRawSync(raw) : raw;
    const local = Buffer.alloc(30);
    local.writeUInt32LE(0x04034b50, 0);
    local.writeUInt16LE(20, 4); // version needed
    local.writeUInt16LE(method, 8);
    local.writeUInt32LE(data.length, 18); // compressed size
    local.writeUInt32LE(raw.length, 22); // uncompressed size
    local.writeUInt16LE(nameBytes.length, 26);
    localParts.push(local, nameBytes, data);

    const central = Buffer.alloc(46);
    central.writeUInt32LE(0x02014b50, 0);
    central.writeUInt16LE(20, 6); // version needed
    central.writeUInt16LE(method, 10);
    central.writeUInt32LE(data.length, 20);
    central.writeUInt32LE(raw.length, 24);
    central.writeUInt16LE(nameBytes.length, 28);
    central.writeUInt32LE(offset, 42); // local header offset
    centralParts.push(central, nameBytes);

    offset += 30 + nameBytes.length + data.length;
  }
  const centralStart = offset;
  const centralBuffer = Buffer.concat(centralParts);
  const eocd = Buffer.alloc(22);
  eocd.writeUInt32LE(0x06054b50, 0);
  eocd.writeUInt16LE(entries.length, 8);
  eocd.writeUInt16LE(entries.length, 10);
  eocd.writeUInt32LE(centralBuffer.length, 12);
  eocd.writeUInt32LE(centralStart, 16);
  return Buffer.concat([...localParts, centralBuffer, eocd]);
}

function tempExtractDir() {
  return fs.mkdtempSync(path.join(os.tmpdir(), "mehen-zip-test-"));
}

test("extractZip extracts stored and deflated entries with nested paths", () => {
  const zip = buildZip([
    { name: "lcov.info", content: "TN:\nSF:a.rs\nDA:1,1\nend_of_record\n" },
    {
      name: "nested/dir/cobertura.xml",
      content: "<coverage/>",
      method: 0,
    },
  ]);
  const dir = tempExtractDir();
  try {
    const files = extractZip(zip, dir);
    assert.deepEqual(
      files.map((f) => path.relative(dir, f)).sort(),
      ["lcov.info", path.join("nested", "dir", "cobertura.xml")].sort(),
    );
    assert.equal(
      fs.readFileSync(path.join(dir, "lcov.info"), "utf8"),
      "TN:\nSF:a.rs\nDA:1,1\nend_of_record\n",
    );
    assert.equal(
      fs.readFileSync(path.join(dir, "nested", "dir", "cobertura.xml"), "utf8"),
      "<coverage/>",
    );
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("extractZip skips directory entries and rejects zip-slip escapes", () => {
  const clean = buildZip([
    { name: "reports/", content: "" },
    { name: "reports/lcov.info", content: "SF:a\nDA:1,1\nend_of_record\n" },
  ]);
  const dir = tempExtractDir();
  try {
    const files = extractZip(clean, dir);
    assert.equal(files.length, 1);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }

  // A hostile entry escaping the destination is an integrity failure,
  // not a degradation case: extraction must throw, never write.
  const hostile = buildZip([{ name: "../evil.txt", content: "boom" }]);
  const dir2 = tempExtractDir();
  try {
    assert.throws(
      () => extractZip(hostile, dir2),
      /escapes the extraction directory/,
    );
    assert.ok(!fs.existsSync(path.join(dir2, "..", "evil.txt")));
  } finally {
    fs.rmSync(dir2, { recursive: true, force: true });
  }
});

test("extractZip rejects non-zip input", () => {
  const dir = tempExtractDir();
  try {
    assert.throws(
      () => extractZip(Buffer.from("definitely not a zip"), dir),
      /not a zip archive/,
    );
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("pickBaseArtifact picks the newest non-expired artifact for the base SHA", () => {
  const baseSha = "a".repeat(40);
  const artifacts = [
    {
      id: 1,
      expired: false,
      created_at: "2026-08-01T00:00:00Z",
      workflow_run: { head_sha: baseSha },
    },
    {
      id: 2,
      expired: false,
      created_at: "2026-08-02T00:00:00Z",
      workflow_run: { head_sha: baseSha },
    },
    // Expired entries and other SHAs never match.
    {
      id: 3,
      expired: true,
      created_at: "2026-08-03T00:00:00Z",
      workflow_run: { head_sha: baseSha },
    },
    {
      id: 4,
      expired: false,
      created_at: "2026-08-04T00:00:00Z",
      workflow_run: { head_sha: "b".repeat(40) },
    },
  ];
  assert.equal(pickBaseArtifact(artifacts, baseSha)?.id, 2);
  assert.equal(pickBaseArtifact(artifacts, "c".repeat(40)), null);
  assert.equal(pickBaseArtifact([], baseSha), null);
  assert.equal(pickBaseArtifact(undefined, baseSha), null);
  assert.equal(pickBaseArtifact(artifacts, ""), null);
});


test("extractZip enforces the decompressed-size budget", () => {
  // Honest metadata over budget: rejected up front from the declared
  // central-directory sizes, before any inflation.
  const big = buildZip([{ name: "big.info", content: "x".repeat(4096) }]);
  const dir = tempExtractDir();
  try {
    assert.throws(
      () => extractZip(big, dir, 1024),
      /declares more than 1024 decompressed bytes/,
    );
    assert.deepEqual(fs.readdirSync(dir), [], "nothing may be written");
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }

  // Lying metadata (the zip-bomb shape): declared sizes pass, but the
  // actual inflate output exceeds the budget — zlib's maxOutputLength
  // aborts mid-inflate instead of allocating the full expansion.
  const bomb = buildZip([{ name: "bomb.info", content: "y".repeat(64 * 1024) }]);
  // Corrupt the declared uncompressed sizes down to 1 byte (central
  // directory offset 24, local header offset 22).
  const cdStart = bomb.readUInt32LE(bomb.length - 22 + 16);
  bomb.writeUInt32LE(1, cdStart + 24);
  bomb.writeUInt32LE(1, 22);
  const dir2 = tempExtractDir();
  try {
    assert.throws(() => extractZip(bomb, dir2, 1024));
    assert.deepEqual(fs.readdirSync(dir2), [], "nothing may be written");
  } finally {
    fs.rmSync(dir2, { recursive: true, force: true });
  }

  // Within budget: extraction is unaffected.
  const fine = buildZip([{ name: "ok.info", content: "SF:a\nDA:1,1\nend_of_record\n" }]);
  const dir3 = tempExtractDir();
  try {
    assert.equal(extractZip(fine, dir3, 1024).length, 1);
  } finally {
    fs.rmSync(dir3, { recursive: true, force: true });
  }
});
