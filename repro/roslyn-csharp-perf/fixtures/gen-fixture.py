#!/usr/bin/env python3
"""Emit a synthetic C# file with N members in one class.

The blow-up scales with *members per type*, not with file length or statement
count, so a generated fixture reproduces it exactly and avoids vendoring
third-party source. Verified against real code: `JsonDocument.Parse.cs`
(953 lines, dotnet/runtime System.Text.Json) takes ~272 s as-published.

    python3 gen-fixture.py 18 > members-18.cs
"""
import sys

n = int(sys.argv[1]) if len(sys.argv) > 1 else 18
print("class C\n{")
for i in range(n):
    print(f"    public int P{i}() {{ return {i}; }}")
print("}")
