// @generated from Roslyn's CSharp.Generated.g4 by prepare-roslyn-grammar.py — do not hand-edit.
// Roslyn publishes a parser-only grammar; this lexer supplies the
// terminals it references. Literal tokens below are harvested from the
// parser's inline literals; the rest is spliced from `lexer-tokens.g4.in`.
// See PROVENANCE.md.
lexer grammar CSharpLexer;

channels { COMMENTS_CHANNEL, DIRECTIVE }

// Emitted only from their lexer modes, but referenced by the parser,
// so they must be declared up front.
tokens { INTERPOLATED_TEXT, XML_TEXT_LIT }

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
OP_078 : '>>>=' ;
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
OP_102 : '"""' ;
OP_103 : '<<=' ;
OP_104 : '>>=' ;
OP_105 : '>>>' ;
OP_106 : '??=' ;
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
OP_120 : '!=' ;
OP_121 : '%=' ;
OP_122 : '&&' ;
OP_123 : '&=' ;
OP_124 : '*=' ;
OP_125 : '++' ;
OP_126 : '+=' ;
OP_127 : '--' ;
OP_128 : '-=' ;
OP_129 : '->' ;
OP_130 : '..' ;
OP_131 : '/=' ;
OP_132 : '/>' ;
OP_133 : '::' ;
OP_134 : '</' ;
OP_135 : '<<' ;
OP_136 : '<=' ;
OP_137 : '==' ;
OP_138 : '=>' ;
OP_139 : '>=' ;
OP_140 : '>>' ;
OP_141 : '??' ;
KW_U8 : 'U8' ;
OP_143 : '\'' ;
OP_144 : '\\' ;
OP_145 : '^=' ;
KW_AS : 'as' ;
KW_BY : 'by' ;
KW_DO : 'do' ;
KW_IF : 'if' ;
KW_IN : 'in' ;
KW_IS : 'is' ;
KW_ON : 'on' ;
KW_OR : 'or' ;
KW_U8_154 : 'u8' ;
OP_155 : '|=' ;
OP_156 : '||' ;
OP_157 : '!' ;
OP_158 : '"' ;
OP_159 : '#' ;
OP_160 : '$' ;
OP_161 : '%' ;
OP_162 : '&' ;
OP_163 : '(' ;
OP_164 : ')' ;
OP_165 : '*' ;
OP_166 : '+' ;
OP_167 : ',' ;
OP_168 : '-' ;
OP_169 : '.' ;
OP_170 : '/' ;
OP_171 : ':' ;
OP_172 : ';' ;
OP_173 : '<' ;
OP_174 : '=' ;
OP_175 : '>' ;
OP_176 : '?' ;
OP_177 : '[' ;
OP_178 : ']' ;
OP_179 : '^' ;
KW__ : '_' ;
OP_181 : '{' ;
OP_182 : '|' ;
OP_183 : '}' ;
OP_184 : '~' ;

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

// §6.4.5.6 Character literals.
CHAR_LIT
    : '\'' ( '\\' . | ~['\\\r\n] ) '\''
    ;

// §6.4.5.7 String literals.
STRING_LIT
    : '"' ( '\\' . | ~["\\\r\n] )* '"'
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
// NOTE: in this frozen snapshot these modes are never entered at all — `$"` is
// harvested as an ordinary literal, so nothing pushes INTERPOLATION. The rules
// below are inert. (The live grammar under `crates/mehen-csharp-parser/` rewrites
// those literals to mode-pushing tokens and does parse interpolated strings; see
// that crate's `grammar/lexer-tokens.g4.in`. This directory is deliberately not
// resynced — it reproduces one timing issue and no fixture uses `$"`.)
//
// The comment below described a design that no longer exists: there is no
// `../src/hooks.rs`, and the live grammar drives every transition from grammar
// commands with the brace depth held in `@lexer::members`. It is kept because the
// *problem* it states is the real one: the `}` that closes a hole is lexically
// identical to the one closing a nested block —
//
//     $"a{ new[]{ 1, 2 }.Length }b"
//                ^^^^^^^^^  must NOT end the hole
//
// so it needs a brace depth per open hole and a *conditional* pop. Keeping the
// grammar free of mode commands also keeps the two halves from fighting over
// the mode stack. See `CSharpLexerHooks` for the state machine.
//
// `prepare-roslyn-grammar.py` rewrites the harvested `'$"'` / `'$@"'` literals
// to the named tokens below (INTERP_TOKEN_LITERALS) so the hook can recognize
// them by a stable name.
INTERP_START          : '$"' ;
INTERP_VERBATIM_START : '$@"' ;

mode INTERPOLATION;

// `{{` / `}}` are escaped literal braces, not holes — first so they win the
// longest match over the single-brace rules below.
INTERP_ESCAPED_OPEN  : '{{' -> type(INTERPOLATED_TEXT) ;
INTERP_ESCAPED_CLOSE : '}}' -> type(INTERPOLATED_TEXT) ;

// Text between holes.
INTERPOLATED_TEXT : ~[{}"\\]+ ;

// A hole opens / the string ends. Both emit the token type the parser expects
// (`{` and `"` respectively); the hook performs the mode change.
INTERP_HOLE_OPEN : '{' -> type(OP_181) ;
INTERP_END       : '"' -> type(OP_158) ;

// A format specifier (`{x:D4}`) is a third mode: after the `:` that ends a
// hole's expression, the remaining text up to the closing `}` is literal format
// text, not C# code — `D4` must not lex as an identifier. (The grammars-v4 C#
// lexer has an INTERPOLATION_FORMAT mode for the same reason.) The hook enters
// this mode on a `:` seen at brace depth 0 inside a hole.
mode INTERPOLATION_FORMAT;

// The format text, emitted as the same token the grammar's
// `interpolation_format_clause : ':' interpolated_string_text_token` expects.
INTERP_FORMAT_TEXT : ~[}"]+ -> type(INTERPOLATED_TEXT) ;

// The `}` that closes the hole; the hook restores the interpolation mode.
INTERP_FORMAT_END : '}' -> type(OP_183) ;
