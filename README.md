# mehen

**mehen** is a Rust-powered CLI for detecting heuristic source code metrics at scale: complexity,
maintainability, lines of code, documentation health, and more.

It is designed for fast, deterministic analysis over large codebases, helping both human and AI
engineers track how complexity evolves over time.

📚 **Documentation: <https://mehen.ophi.dev>**

## What is Mehen?

In Ophidiarium projects, names matter. **Mehen** is a mythical ancient Egyptian serpent associated with
guarding Ra. In the same spirit, `mehen` helps guard your codebase from slowly collapsing under
complexity.

## Why teams use mehen

- **Polyglot by design** — per-file language detection across eleven source languages plus Markdown
  and SQL. Useful for monorepos.
- **Real language parsers** — Ruff for Python, Oxc for TS/JS/JSX/TSX, Mago for PHP, Prism for Ruby,
  `ra_ap_syntax` for Rust, ANTLR (Kotlin spec grammar for Kotlin, grammars-v4 for Java and C#),
  pulldown-cmark for Markdown, sqruff for SQL, tree-sitter for Go, C, PowerShell.
- **Code, documentation, and SQL in one tool** — source-code complexity, Markdown documentation
  health, *and* a dedicated relational metric family for `.sql` files.
- **Deterministic, no network** — pure static analysis. Same input → same output. Safe for air-gapped
  CI.
- **Pull-request native** — built-in `mehen diff` plus a sticky comment GitHub Action.

## Install

```bash
# npm
npm install -g mehen

# PyPI / uv
uv tool install mehen
# or: pip install mehen

# cargo binstall
cargo binstall --git https://github.com/ophi-dev/mehen mehen
```

Full installation guide: <https://mehen.ophi.dev/installation>.

## Quick start

```bash
# Analyze a single file
mehen metrics src/main.py --pretty

# Rank the worst offenders in a tree
mehen top-offenders src --metric cognitive

# Diff metrics against main
mehen diff --from main --to HEAD --paths src --output-format markdown
```

Quickstart: <https://mehen.ophi.dev/quickstart>.

## GitHub Action

Drop the action into a workflow to publish per-PR metric trends:

```yaml
permissions:
  contents: read
  pull-requests: write
  issues: write

steps:
  - uses: actions/checkout@v6
    with:
      fetch-depth: 0
  - uses: ophi-dev/mehen@v1
    with:
      paths: src
```

Full reference: <https://mehen.ophi.dev/guides/github-action>.

## Documentation

Everything else lives in the docs site:

- [Code metrics](https://mehen.ophi.dev/metrics/code/overview) — cyclomatic, cognitive, Halstead, MI,
  ABC, LOC family, NOM, NPA, NPM, WMC.
- [Markdown metrics](https://mehen.ophi.dev/metrics/markdown/overview) — DMI, MRPC, MCC, link debt,
  filler/lazy risk, English/Japanese prose layer.
- [SQL metrics](https://mehen.ophi.dev/metrics/sql/overview) — CTE graphs, join/subquery structure,
  object-touch risk, SQL Halstead, and composite scores via `mehen-sql` (sqruff-backed).
- [Commands](https://mehen.ophi.dev/commands/overview) — `mehen metrics`, `mehen diff`,
  `mehen top-offenders`.
- [Developers guide](https://mehen.ophi.dev/developers/overview) — build, test, contribute, add a
  language.

## Contributing

Issues and pull requests welcome at <https://github.com/ophi-dev/mehen/issues>.

## License

`mehen` is released under the [GNU Affero General Public License v3.0](https://www.gnu.org/licenses/agpl-3.0.html).
