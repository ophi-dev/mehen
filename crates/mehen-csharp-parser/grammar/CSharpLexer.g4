// @generated from Roslyn's CSharp.Generated.g4 by prepare-grammar.py — do not hand-edit.
// Roslyn publishes a parser-only grammar; this lexer supplies the
// terminals it references. Literal tokens below are harvested from the
// parser's inline literals; the rest is spliced from `lexer-tokens.g4.in` and `lexer-members.g4.in`.
// See PROVENANCE.md.
lexer grammar CSharpLexer;

channels { COMMENTS_CHANNEL, DIRECTIVE }

// Emitted only from their lexer modes, but referenced by the parser,
// so they must be declared up front.
tokens { INTERPOLATED_TEXT, XML_TEXT_LIT }

// Interpolated-string state. Kept in the grammar rather than a hand-written Rust
// hook so the lexer is self-describing; `prepare-grammar.py` emits matching
// `[[member]]` / `[[pattern]]` declarations into `patterns.toml`, which lower
// every body below to pure SemIR (runtime 0.20.1+, upstream #206).
//
//   holeStack — one entry per interpolation hole currently open, holding the
//               brace-nesting depth *within* that hole. Pushed when a hole opens,
//               popped when it closes, so it survives nesting like
//               `$"{ $"{x}" }"`. Its depth is nonzero exactly while inside a
//               hole's expression.
//   nestDepth — mirror of the innermost hole's depth, kept as a scalar because
//               the pattern DSL can test a scalar's truthiness directly.
//
// Two slots rather than one because the lowering DSL has only `not` and
// truthiness — no comparisons and no `&&`. Each slot must therefore answer one
// yes/no question on its own, and the *conjunction* comes from rule order: the
// deeper case is written first, so reaching a later rule already implies the
// earlier predicate was false.
@lexer::members
{private int nestDepth;
private Stack<int> holeStack = new Stack<int>();
}

// ---- keywords, operators, punctuation (must precede IDENTIFIER) ----
KW___REFVALUE : '__refvalue' ;
KW_DESCENDING : 'descending' ;
KW_STACKALLOC : 'stackalloc' ;
KW___ARGLIST : '__arglist' ;
KW___MAKEREF : '__makeref' ;
KW___REFTYPE : '__reftype' ;
KW_ASCENDING : 'ascending' ;
KW_EXTENSION : 'extension' ;
KW_INTERFACE : 'interface' ;
KW_NAMESPACE : 'namespace' ;
KW_PROTECTED : 'protected' ;
KW_UNCHECKED : 'unchecked' ;
KW_UNMANAGED : 'unmanaged' ;
KW_ABSTRACT : 'abstract' ;
KW_CONTINUE : 'continue' ;
KW_DELEGATE : 'delegate' ;
KW_EXPLICIT : 'explicit' ;
KW_IMPLICIT : 'implicit' ;
KW_INTERNAL : 'internal' ;
KW_OPERATOR : 'operator' ;
KW_OVERRIDE : 'override' ;
KW_READONLY : 'readonly' ;
KW_REQUIRED : 'required' ;
KW_VOLATILE : 'volatile' ;
KW_CHECKED : 'checked' ;
KW_DECIMAL : 'decimal' ;
KW_DEFAULT : 'default' ;
KW_FINALLY : 'finally' ;
KW_FOREACH : 'foreach' ;
KW_MANAGED : 'managed' ;
KW_ORDERBY : 'orderby' ;
KW_PARTIAL : 'partial' ;
KW_PRIVATE : 'private' ;
KW_VIRTUAL : 'virtual' ;
KW_ALLOWS : 'allows' ;
KW_CLOSED : 'closed' ;
KW_DOUBLE : 'double' ;
KW_EQUALS : 'equals' ;
KW_EXTERN : 'extern' ;
KW_GLOBAL : 'global' ;
KW_OBJECT : 'object' ;
KW_PARAMS : 'params' ;
KW_PUBLIC : 'public' ;
KW_REMOVE : 'remove' ;
KW_RETURN : 'return' ;
KW_SCOPED : 'scoped' ;
KW_SEALED : 'sealed' ;
KW_SELECT : 'select' ;
KW_SIZEOF : 'sizeof' ;
KW_STATIC : 'static' ;
KW_STRING : 'string' ;
KW_STRUCT : 'struct' ;
KW_SWITCH : 'switch' ;
KW_TYPEOF : 'typeof' ;
KW_UNSAFE : 'unsafe' ;
KW_USHORT : 'ushort' ;
KW_ALIAS : 'alias' ;
KW_ASYNC : 'async' ;
KW_AWAIT : 'await' ;
KW_BREAK : 'break' ;
KW_CATCH : 'catch' ;
KW_CLASS : 'class' ;
KW_CONST : 'const' ;
KW_EVENT : 'event' ;
KW_FALSE : 'false' ;
KW_FIELD : 'field' ;
KW_FIXED : 'fixed' ;
KW_FLOAT : 'float' ;
KW_GROUP : 'group' ;
KW_SBYTE : 'sbyte' ;
KW_SHORT : 'short' ;
KW_THROW : 'throw' ;
KW_ULONG : 'ulong' ;
KW_UNION : 'union' ;
KW_USING : 'using' ;
KW_WHERE : 'where' ;
KW_WHILE : 'while' ;
KW_YIELD : 'yield' ;
KW_BASE : 'base' ;
KW_BOOL : 'bool' ;
KW_BYTE : 'byte' ;
KW_CASE : 'case' ;
KW_CHAR : 'char' ;
KW_ELSE : 'else' ;
KW_ENUM : 'enum' ;
KW_FILE : 'file' ;
KW_FROM : 'from' ;
KW_GOTO : 'goto' ;
KW_INIT : 'init' ;
KW_INTO : 'into' ;
KW_JOIN : 'join' ;
KW_LOCK : 'lock' ;
KW_LONG : 'long' ;
KW_NULL : 'null' ;
KW_SAFE : 'safe' ;
KW_THIS : 'this' ;
KW_TRUE : 'true' ;
KW_UINT : 'uint' ;
KW_VOID : 'void' ;
KW_WHEN : 'when' ;
KW_WITH : 'with' ;
OP_101 : '"""' ;
OP_102 : '<<=' ;
OP_103 : '??=' ;
KW_ADD : 'add' ;
KW_AND : 'and' ;
KW_FOR : 'for' ;
KW_GET : 'get' ;
KW_INT : 'int' ;
KW_LET : 'let' ;
KW_NEW : 'new' ;
KW_NOT : 'not' ;
KW_OUT : 'out' ;
KW_REF : 'ref' ;
KW_SET : 'set' ;
KW_TRY : 'try' ;
KW_VAR : 'var' ;
OP_117 : '!=' ;
OP_118 : '%=' ;
OP_119 : '&&' ;
OP_120 : '&=' ;
OP_121 : '*=' ;
OP_122 : '++' ;
OP_123 : '+=' ;
OP_124 : '--' ;
OP_125 : '-=' ;
OP_126 : '->' ;
OP_127 : '..' ;
OP_128 : '/=' ;
OP_129 : '/>' ;
OP_130 : '::' ;
OP_131 : '</' ;
OP_132 : '<<' ;
OP_133 : '<=' ;
OP_134 : '==' ;
OP_135 : '=>' ;
OP_136 : '>=' ;
OP_137 : '??' ;
KW_U8 : 'U8' ;
OP_139 : '\'' ;
OP_140 : '\\' ;
OP_141 : '^=' ;
KW_AS : 'as' ;
KW_BY : 'by' ;
KW_DO : 'do' ;
KW_IF : 'if' ;
KW_IN : 'in' ;
KW_IS : 'is' ;
KW_ON : 'on' ;
KW_OR : 'or' ;
KW_U8_150 : 'u8' ;
OP_151 : '|=' ;
OP_152 : '||' ;
OP_153 : '!' ;
DQUOTE : '"' ;
OP_155 : '#' ;
OP_156 : '$' ;
OP_157 : '%' ;
OP_158 : '&' ;
OP_159 : '(' ;
OP_160 : ')' ;
OP_161 : '*' ;
OP_162 : '+' ;
OP_163 : ',' ;
OP_164 : '-' ;
OP_165 : '.' ;
OP_166 : '/' ;
OP_168 : ';' ;
OP_169 : '<' ;
OP_170 : '=' ;
OP_171 : '>' ;
OP_172 : '?' ;
OP_173 : '[' ;
OP_174 : ']' ;
OP_175 : '^' ;
KW__ : '_' ;
OP_178 : '|' ;
OP_180 : '~' ;

// Lexer rules supplied for Roslyn's parser-only C# grammar, spliced verbatim
// into the generated `CSharpLexer.g4` by `prepare-roslyn-grammar.py`.
//
// Roslyn's `CSharp.Generated.g4` describes its terminals as character-level
// *parser* rules (`identifier_token : '@'? identifier_start_character …`,
// `decimal_digit : '0' | '1' | …`). Those cannot stay in the parser: single
// character tokens would shadow multi-character ones, so `'C'` beats
// `IDENTIFIER` and `'1'` beats a decimal literal. Each rule below replaces one
// such Roslyn rule, following the C# lexical grammar (ECMA-334 §6.4).
//
// This file is hand-written ANTLR (not generated), kept separate from the
// script so the ANTLR-level escaping is readable and reviewable as grammar
// source rather than as nested Python string escapes.
//
// The `TOKEN <-> roslyn_rule` mapping is declared in the script's
// LEXER_TOKEN_RULES table; adding a rule here requires adding it there too.

// §6.4.3 Identifiers. `@` is the verbatim-identifier prefix; the character
// classes follow identifier_start_character / identifier_part_character.
IDENTIFIER
    : '@'? [\p{L}\p{Nl}_] [\p{L}\p{Nl}\p{Nd}\p{Mn}\p{Mc}\p{Pc}\p{Cf}]*
    ;

// §6.4.5.3 Integer literals. Two suffix slots cover `ul` / `lu`.
DEC_INT_LIT
    : [0-9] [0-9_]* [uUlL]? [uUlL]?
    ;

HEX_INT_LIT
    : '0' [xX] [0-9a-fA-F_]+ [uUlL]? [uUlL]?
    ;

BIN_INT_LIT
    : '0' [bB] [01_]+ [uUlL]? [uUlL]?
    ;

// §6.4.5.5 Real literals — embedded dot, leading dot, exponent-only, and
// suffix-only forms.
REAL_LIT
    : [0-9] [0-9_]* '.' [0-9] [0-9_]* ExponentPart? [fFdDmM]?
    | '.' [0-9] [0-9_]* ExponentPart? [fFdDmM]?
    | [0-9] [0-9_]* ExponentPart [fFdDmM]?
    | [0-9] [0-9_]* [fFdDmM]
    ;

fragment ExponentPart
    : [eE] [+-]? [0-9] [0-9_]*
    ;

// §6.4.5.6 Character literals. A char literal holds exactly one character, so
// unlike STRING_LIT below it has no closure to absorb a mis-sized escape: the
// escape forms have to be spelled out, or `'\ud800'` matches `\u` and then looks
// for the closing quote at `d`.
CHAR_LIT
    : '\'' ( Escape | ~['\\\r\n] ) '\''
    ;

// §6.4.5.6 escape sequences: simple (`\n`, `\\`, `\'`), hex (`\xA` .. `\xABCD`),
// and unicode — four hex digits after `u`, eight after `U`.
fragment Escape
    : '\\' ( 'u' HexQuad | 'U' HexQuad HexQuad | 'x' [0-9a-fA-F]+ | . )
    ;

fragment HexQuad
    : [0-9a-fA-F] [0-9a-fA-F] [0-9a-fA-F] [0-9a-fA-F]
    ;

// §6.4.5.7 String literals. The `*` closure means a mis-sized escape still lexes
// (the remaining characters match the negated set), but spell the escapes anyway
// so the token boundary is right for `"\\"` and friends.
STRING_LIT
    : '"' ( Escape | ~["\\\r\n] )* '"'
    ;

VERBATIM_STRING_LIT
    : '@"' ( '""' | ~'"' )* '"'
    ;

// C# 11 raw string literals. The real rule requires the closing fence to be at
// least as long as the opening one, which a context-free lexer rule cannot
// express; a non-greedy match is sufficient here because every metric that
// touches a string literal (LOC rows, Halstead operand) only needs the token's
// extent, not its internal structure.
ML_RAW_STRING_LIT
    : '"""' '"'* .*? '"""' '"'*
    ;

SL_RAW_STRING_LIT
    : '""' ~[\r\n]*? '""'
    ;

// ---- trivia -------------------------------------------------------------
// Comments go to a dedicated channel so the CLOC sweep can read them while the
// parser never sees them. Roslyn models trivia as syntax, so its grammar has no
// rules for these at all.
SINGLE_LINE_DOC_COMMENT : '///' ~[\r\n]*  -> channel(COMMENTS_CHANNEL) ;
DELIMITED_DOC_COMMENT   : '/**' .*? '*/'  -> channel(COMMENTS_CHANNEL) ;
SINGLE_LINE_COMMENT     : '//'  ~[\r\n]*  -> channel(COMMENTS_CHANNEL) ;
DELIMITED_COMMENT       : '/*'  .*? '*/'  -> channel(COMMENTS_CHANNEL) ;
WHITESPACES             : [ \t\r\n\f]+    -> channel(HIDDEN) ;
BYTE_ORDER_MARK         : '﻿'        -> skip ;

// A preprocessor directive line. mehen does not evaluate `#if` (unlike the
// grammars-v4 lexer's `CSharpLexerBase`, which needs a stateful hook for it);
// directives are routed to their own channel so they are neither code nor
// comment for LOC, and inactive regions are still parsed as ordinary code.
DIRECTIVE_LINE          : '#' ~[\r\n]*    -> channel(DIRECTIVE) ;

// ---- interpolated strings ----------------------------------------------
// Roslyn spells interpolated strings as
//
//     interpolated_string_expression
//       : '$"'  interpolated_string_content* '"'
//       | '$@"' interpolated_string_content* '"' ;
//     interpolation : '{' expression … '}' ;
//
// The *text* between holes needs its own lexer mode: in the default mode a
// broad negated set would swallow ordinary code (an earlier flat-lexer attempt
// lexed `class C ` as one token that way).
//
// The state and every mode transition live in the grammar. The one genuinely
// hard decision is the `}` that closes a hole, which is lexically identical to
// the one closing a nested block:
//
//     $"a{ new[]{ 1, 2 }.Length }b"
//                ^^^^^^^^^  must NOT end the hole
//
// SemIR has no conditional and no mode-changing *action* (its statements only
// touch member state), so this cannot be one rule with an `if`. Instead the two
// meanings become two rules over the same character, each gated by a predicate
// on the brace depth and each carrying its own *unconditional* command. The
// lexer evaluates predicates during ATN simulation, so the rule choice does the
// work the missing conditional would:
//
//     INTERP_NESTED_CLOSE : {nestDepth > 0}?      '}' { nestDepth--; }
//     INTERP_HOLE_CLOSE   : {holeStack.Count > 0}? '}' -> popMode
//
// Stack-valued member state is runtime 0.20.1+ (upstream #206, filed for exactly
// this shape).
//
// Regular vs. verbatim needs no state: `$"` and `$@"` push *different* text
// modes, so the mode itself records the flavour.
//
// `prepare-grammar.py` rewrites the harvested `'$"'` / `'$@"'` literals to the
// named tokens below (INTERP_TOKEN_LITERALS). `LBRACE` / `RBRACE` / `DQUOTE` /
// `COLON` are harvested literals the script pins to those names
// (STABLE_TOKEN_NAMES) so the `type(…)` commands here stay valid — the default
// `OP_nnn` names are index-derived and shift whenever the literal set changes.
INTERP_START          : '$"'  -> pushMode(INTERPOLATION) ;
INTERP_VERBATIM_START : '$@"' -> pushMode(INTERPOLATION_VERBATIM) ;

// ---- braces and `:` inside an interpolation hole -----------------------
// `{`, `}` and `:` mean different things inside a hole, so this file defines them
// rather than letting the script harvest them into the literals block: ANTLR
// breaks an equal-length match by rule order, so the gated rules have to come
// first and the plain fallbacks last. (HOLE_SENSITIVE_LITERALS in the script
// keeps the harvester from emitting duplicates.)
//
// Order is load-bearing twice over. Between the gated rules, the DSL cannot
// express `inHole && depth == 0`, so the deeper case goes first: if `nestDepth`
// is nonzero we are certainly inside a hole, and reaching the next rule proves
// `nestDepth == 0`. And all of them precede the unguarded fallbacks, which are
// what ordinary code outside any hole matches.

// Nested block/initializer brace — unwind one level, stay in the hole.
INTERP_NESTED_CLOSE
    : {nestDepth > 0}? '}' { nestDepth--; } -> type(RBRACE)
    ;

// `nestDepth` is 0 here, so a hole is open and this `}` closes it. Popping
// `holeStack` restores the *enclosing* hole's depth, which matters for
// `$"{ $"{x}" }"`: the inner hole's count must not clobber the outer one's.
INTERP_HOLE_CLOSE
    : {holeStack.Count > 0}? '}' { nestDepth = holeStack.Pop(); } -> type(RBRACE), popMode
    ;

// An opening brace inside a hole deepens the count.
INTERP_NESTED_OPEN
    : {holeStack.Count > 0}? '{' { nestDepth++; } -> type(LBRACE)
    ;

// `{x:D4}` — a `:` at depth 0 inside a hole starts the format specifier, so the
// rest of the hole is literal text. Guarded by `nestDepth` first for the same
// reason: a `:` at any deeper level belongs to a nested construct (a ternary, a
// dictionary initializer, a label) and must stay an ordinary `COLON`.
INTERP_NESTED_COLON
    : {nestDepth > 0}? ':' -> type(COLON)
    ;

INTERP_FORMAT_COLON
    : {holeStack.Count > 0}? ':' -> type(COLON), pushMode(INTERPOLATION_FORMAT)
    ;

// The unguarded fallbacks, reached only when every predicate above was false —
// i.e. ordinary code outside any interpolation hole. Their names are pinned by
// STABLE_TOKEN_NAMES so the `type(…)` commands above stay valid.
LBRACE : '{' ;
RBRACE : '}' ;
COLON  : ':' ;

mode INTERPOLATION;

// `{{` / `}}` are escaped literal braces, not holes — first so they win the
// longest match over the single-brace rules below.
INTERP_ESCAPED_OPEN  : '{{' -> type(INTERPOLATED_TEXT) ;
INTERP_ESCAPED_CLOSE : '}}' -> type(INTERPOLATED_TEXT) ;

// Text between holes. A backslash starts an escape sequence, so `\"` does not
// end the string and must be consumed as a unit.
INTERP_ESCAPE     : '\\' . -> type(INTERPOLATED_TEXT) ;
INTERPOLATED_TEXT : ~[{}"\\]+ ;

// A hole opens: its expression is ordinary C#, so switch to the default mode and
// start counting braces for this hole.
INTERP_HOLE_OPEN : '{' { holeStack.Push(nestDepth); nestDepth = 0; } -> type(LBRACE), pushMode(DEFAULT_MODE) ;

// The string ends: drop this string's entry and leave the text mode.
INTERP_END : '"' -> type(DQUOTE), popMode ;

// A *verbatim* interpolated string (`$@"…"`) has different lexical rules for the
// same syntax: a backslash is an ordinary character (no escape sequences) and a
// doubled `""` is the escaped quote. One text rule cannot serve both flavours,
// so `$@"` pushes this mode instead. (The grammars-v4 C# lexer splits the same
// two cases for the same reason.) Braces and holes behave identically, so these
// rules mirror the ones above and emit the same token types.
mode INTERPOLATION_VERBATIM;

INTERP_V_ESCAPED_OPEN  : '{{' -> type(INTERPOLATED_TEXT) ;
INTERP_V_ESCAPED_CLOSE : '}}' -> type(INTERPOLATED_TEXT) ;

// `""` is a literal quote inside a verbatim string, so it must not end it.
INTERP_V_ESCAPED_QUOTE : '""' -> type(INTERPOLATED_TEXT) ;
INTERP_V_TEXT          : ~[{}"]+ -> type(INTERPOLATED_TEXT) ;

INTERP_V_HOLE_OPEN : '{' { holeStack.Push(nestDepth); nestDepth = 0; } -> type(LBRACE), pushMode(DEFAULT_MODE) ;
INTERP_V_END       : '"' -> type(DQUOTE), popMode ;

// A format specifier (`{x:D4}`) is a third mode: after the `:` that ends a
// hole's expression, the remaining text up to the closing `}` is literal format
// text, not C# code — `D4` must not lex as an identifier. (The grammars-v4 C#
// lexer has an INTERPOLATION_FORMAT mode for the same reason.) Entered from the
// default mode by the `:` rule below, at brace depth 0 inside a hole.
mode INTERPOLATION_FORMAT;

// The format text, emitted as the same token the grammar's
// `interpolation_format_clause : ':' interpolated_string_text_token` expects.
INTERP_FORMAT_TEXT : ~[}"]+ -> type(INTERPOLATED_TEXT) ;

// The `}` that closes the hole. Two pops: this mode, then the hole's own
// DEFAULT_MODE, landing back in the enclosing interpolation text mode.
INTERP_FORMAT_END : '}' -> type(RBRACE), popMode, popMode ;
