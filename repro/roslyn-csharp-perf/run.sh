#!/usr/bin/env bash
# One-command reproduction of the Roslyn-grammar member-scaling blow-up.
#
#   ./run.sh slow    # as-published: record keyword is the catch-all `syntax_token`
#   ./run.sh fixed   # record contextual keyword restored (default)
#
# Needs `antlr4-rust-gen` 0.33.1 on PATH — it must MATCH the runtime version
# pinned in Cargo.toml, or the generated modules call a different API than the
# crate they link against (e.g. the 0.23 `SyntaxErrorEvent` change):
#   cargo install antlr-rust-codegen --version 0.33.1 \
#       --bin antlr4-rust-gen --force
set -euo pipefail
cd "$(dirname "$0")"

VARIANT="${1:-fixed}"
# ANTLR requires the file name to match the grammar name, so each variant
# lives in its own directory with identically-named files.
case "$VARIANT" in
  fixed)  GDIR=grammar ;;                          # record contextual keyword restored
  slow)   GDIR=grammar/unnarrowed-record ;;        # as-published: keyword is `syntax_token`
  *) echo "usage: $0 [fixed|slow]" >&2; exit 2 ;;
esac

command -v antlr4-rust-gen >/dev/null || {
  echo "antlr4-rust-gen not on PATH — see the header of this script" >&2
  exit 1
}

echo "== generating ($VARIANT) =="
rm -rf src/generated && mkdir -p src/generated
# `--sem-unknown error --require-full-semantics` mirrors how mehen generates:
# nothing may be silently assumed. The grammar needs no hooks or patterns.
antlr4-rust-gen "$GDIR/CSharpLexer.g4" "$GDIR/CSharpParser.g4" \
  --out-dir src/generated --sem-patterns "$GDIR/patterns.toml" \
  --sem-unknown error --require-full-semantics

echo "== building =="
cargo build --release --quiet --bin time-parse

echo "== members-per-class scaling =="
mkdir -p target/fixtures
for n in 4 8 12 18 24; do
  python3 fixtures/gen-fixture.py "$n" > "target/fixtures/members-$n.cs"
done
printf '%s\n' "  (elapsed ms / recovered errors / fixture)"
./target/release/time-parse target/fixtures/members-*.cs

echo "== regression: omitted-node syntax must parse with 0 errors =="
# `--require-clean` makes a nonzero error count exit 1, so `set -e` actually
# enforces the assertion this step advertises. Without it `time-parse` printed
# the count and exited 0, so a broken grammar passed silently.
./target/release/time-parse --require-clean fixtures/omitted-nodes.cs

if [ "$#" -gt 1 ]; then
  shift
  echo "== extra files =="
  ./target/release/time-parse "$@"
fi
