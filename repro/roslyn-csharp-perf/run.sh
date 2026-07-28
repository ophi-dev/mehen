#!/usr/bin/env bash
# One-command reproduction of the Roslyn-grammar member-scaling blow-up.
#
#   ./run.sh              # as-published grammar (slow, quadratic)
#   ./run.sh braces       # with body braces made required (flat)
#
# Needs `antlr4-rust-gen` 0.21.0 on PATH:
#   cargo install antlr-rust-runtime --version 0.21.0 \
#       --features codegen --bin antlr4-rust-gen --force
set -euo pipefail
cd "$(dirname "$0")"

VARIANT="${1:-as-published}"
# ANTLR requires the file name to match the grammar name, so each variant
# lives in its own directory with identically-named files.
case "$VARIANT" in
  as-published) GDIR=grammar ;;
  braces)       GDIR=grammar/braces-required ;;
  *) echo "usage: $0 [as-published|braces]" >&2; exit 2 ;;
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
  --out-dir src/generated \
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

if [ "$#" -gt 1 ]; then
  shift
  echo "== extra files =="
  ./target/release/time-parse "$@"
fi
