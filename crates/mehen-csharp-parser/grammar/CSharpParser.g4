// @generated from Roslyn's CSharp.Generated.g4 by prepare-roslyn-grammar.py — do not hand-edit.
// See PROVENANCE.md for the pinned upstream revision and the patch rationale.
parser grammar CSharpParser;

options { tokenVocab=CSharpLexer; }

compilation_unit
  : extern_alias_directive* using_directive* attribute_list* member_declaration*
  ;

extern_alias_directive
  : KW_EXTERN KW_ALIAS identifier_token OP_168
  ;

using_directive
  : KW_GLOBAL? KW_USING (KW_STATIC | (KW_UNSAFE? name_equals))? type OP_168
  ;

name_equals
  : identifier_name OP_170
  ;

identifier_name
  : KW_GLOBAL
  | identifier_token
  ;

attribute_list
  : OP_173 attribute_target_specifier? attribute (OP_163 attribute)* OP_174
  ;

attribute_target_specifier
  : syntax_token COLON
  ;

attribute
  : name attribute_argument_list?
  ;

name
  : alias_qualified_name
  | qualified_name
  | simple_name
  ;

alias_qualified_name
  : identifier_name OP_130 simple_name
  ;

simple_name
  : generic_name
  | identifier_name
  ;

generic_name
  : identifier_token type_argument_list
  ;

type_argument_list
  : OP_169 (type? (OP_163 type?)*)? OP_171
  ;

qualified_name
  : name OP_165 simple_name
  ;

attribute_argument_list
  : OP_159 (attribute_argument (OP_163 attribute_argument)*)? OP_160
  ;

attribute_argument
  : (name_equals? | name_colon?) expression
  ;

name_colon
  : identifier_name COLON
  ;

member_declaration
  : base_field_declaration
  | base_method_declaration
  | base_namespace_declaration
  | base_property_declaration
  | base_type_declaration
  | delegate_declaration
  | enum_member_declaration
  | global_statement
  | incomplete_member
  ;

base_field_declaration
  : event_field_declaration
  | field_declaration
  ;

event_field_declaration
  : attribute_list* modifier* KW_EVENT variable_declaration OP_168
  ;

modifier
  : KW_ABSTRACT
  | KW_ASYNC
  | KW_CLOSED
  | KW_CONST
  | KW_EXTERN
  | KW_FILE
  | KW_FIXED
  | KW_INTERNAL
  | KW_NEW
  | KW_OVERRIDE
  | KW_PARTIAL
  | KW_PRIVATE
  | KW_PROTECTED
  | KW_PUBLIC
  | KW_READONLY
  | KW_REF
  | KW_REQUIRED
  | KW_SAFE
  | KW_SCOPED
  | KW_SEALED
  | KW_STATIC
  | KW_UNSAFE
  | KW_VIRTUAL
  | KW_VOLATILE
  ;

variable_declaration
  : type variable_declarator (OP_163 variable_declarator)*
  ;

variable_declarator
  : identifier_token bracketed_argument_list? equals_value_clause?
  ;

bracketed_argument_list
  : OP_173 argument (OP_163 argument)* OP_174
  ;

argument
  : name_colon? (KW_REF | KW_OUT | KW_IN)? expression
  ;

equals_value_clause
  : OP_170 expression
  ;

field_declaration
  : attribute_list* modifier* variable_declaration OP_168
  ;

base_method_declaration
  : constructor_declaration
  | conversion_operator_declaration
  | destructor_declaration
  | method_declaration
  | operator_declaration
  ;

constructor_declaration
  : attribute_list* modifier* identifier_token parameter_list constructor_initializer? (block | (arrow_expression_clause OP_168))
  ;

parameter_list
  : OP_159 (parameter (OP_163 parameter)*)? OP_160
  ;

parameter
  : attribute_list* (modifier | KW_OUT | KW_IN | KW_PARAMS | KW_THIS)* type? (identifier_token | KW___ARGLIST)? equals_value_clause?
  ;

constructor_initializer
  : COLON (KW_BASE | KW_THIS) argument_list
  ;

argument_list
  : OP_159 (argument (OP_163 argument)*)? OP_160
  ;

block
  : attribute_list* LBRACE statement* RBRACE
  ;

arrow_expression_clause
  : OP_135 expression
  ;

conversion_operator_declaration
  : attribute_list* modifier* (KW_IMPLICIT | KW_EXPLICIT) explicit_interface_specifier? KW_OPERATOR KW_CHECKED? type parameter_list (block | (arrow_expression_clause OP_168))
  ;

explicit_interface_specifier
  : name OP_165
  ;

destructor_declaration
  : attribute_list* modifier* OP_180 identifier_token parameter_list (block | (arrow_expression_clause OP_168))
  ;

method_declaration
  : attribute_list* modifier* type explicit_interface_specifier? identifier_token type_parameter_list? parameter_list type_parameter_constraint_clause* (block | (arrow_expression_clause OP_168))
  ;

type_parameter_list
  : OP_169 type_parameter (OP_163 type_parameter)* OP_171
  ;

type_parameter
  : attribute_list* (KW_IN | KW_OUT)? identifier_token
  ;

type_parameter_constraint_clause
  : KW_WHERE identifier_name COLON type_parameter_constraint (OP_163 type_parameter_constraint)*
  ;

type_parameter_constraint
  : allows_constraint_clause
  | class_or_struct_constraint
  | constructor_constraint
  | default_constraint
  | type_constraint
  ;

allows_constraint_clause
  : KW_ALLOWS allows_constraint (OP_163 allows_constraint)*
  ;

allows_constraint
  : ref_struct_constraint
  ;

ref_struct_constraint
  : KW_REF KW_STRUCT
  ;

class_or_struct_constraint
  : KW_CLASS OP_172?
  | KW_STRUCT OP_172?
  ;

constructor_constraint
  : KW_NEW OP_159 OP_160
  ;

default_constraint
  : KW_DEFAULT
  ;

type_constraint
  : type
  ;

operator_declaration
  : attribute_list* modifier* type explicit_interface_specifier? KW_OPERATOR KW_CHECKED? (OP_162 | OP_164 | OP_153 | OP_180 | OP_122 | OP_124 | OP_161 | OP_166 | OP_157 | OP_132 | right_shift | unsigned_right_shift | OP_178 | OP_158 | OP_175 | OP_134 | OP_117 | OP_169 | OP_133 | OP_171 | OP_136 | KW_FALSE | KW_TRUE | KW_IS | OP_123 | OP_125 | OP_121 | OP_128 | OP_118 | OP_120 | OP_151 | OP_141 | OP_102 | right_shift_assignment | unsigned_right_shift_assignment) parameter_list (block | (arrow_expression_clause OP_168))
  ;

base_namespace_declaration
  : file_scoped_namespace_declaration
  | namespace_declaration
  ;

file_scoped_namespace_declaration
  : attribute_list* modifier* KW_NAMESPACE name OP_168 extern_alias_directive* using_directive* member_declaration*
  ;

namespace_declaration
  : attribute_list* modifier* KW_NAMESPACE name LBRACE extern_alias_directive* using_directive* member_declaration* RBRACE OP_168?
  ;

base_property_declaration
  : event_declaration
  | indexer_declaration
  | property_declaration
  ;

event_declaration
  : attribute_list* modifier* KW_EVENT type explicit_interface_specifier? identifier_token (accessor_list | OP_168)
  ;

accessor_list
  : LBRACE accessor_declaration* RBRACE
  ;

accessor_declaration
  : attribute_list* modifier* (KW_GET | KW_SET | KW_INIT | KW_ADD | KW_REMOVE | identifier_token) (block | (arrow_expression_clause OP_168) | OP_168)
  ;

indexer_declaration
  : attribute_list* modifier* type explicit_interface_specifier? KW_THIS bracketed_parameter_list (accessor_list | (arrow_expression_clause OP_168))
  ;

bracketed_parameter_list
  : OP_173 parameter (OP_163 parameter)* OP_174
  ;

property_declaration
  : attribute_list* modifier* type explicit_interface_specifier? identifier_token (accessor_list (equals_value_clause OP_168)? | ((arrow_expression_clause | equals_value_clause) OP_168))
  ;

base_type_declaration
  : enum_declaration
  | type_declaration
  ;

enum_declaration
  : attribute_list* modifier* KW_ENUM identifier_token base_list? (LBRACE (enum_member_declaration (OP_163 enum_member_declaration)* OP_163?)? RBRACE)? OP_168?
  ;

base_list
  : COLON base_type (OP_163 base_type)*
  ;

base_type
  : primary_constructor_base_type
  | simple_base_type
  ;

primary_constructor_base_type
  : type argument_list
  ;

simple_base_type
  : type
  ;

enum_member_declaration
  : attribute_list* modifier* identifier_token equals_value_clause?
  ;

type_declaration
  : class_declaration
  | extension_block_declaration
  | interface_declaration
  | record_declaration
  | struct_declaration
  | union_declaration
  ;

class_declaration
  : attribute_list* modifier* KW_CLASS identifier_token type_parameter_list? parameter_list? base_list? type_parameter_constraint_clause* (LBRACE member_declaration* RBRACE)? OP_168?
  ;

extension_block_declaration
  : attribute_list* modifier* KW_EXTENSION type_parameter_list? parameter_list? type_parameter_constraint_clause* (LBRACE member_declaration* RBRACE)? OP_168?
  ;

interface_declaration
  : attribute_list* modifier* KW_INTERFACE identifier_token type_parameter_list? parameter_list? base_list? type_parameter_constraint_clause* (LBRACE member_declaration* RBRACE)? OP_168?
  ;

record_declaration
  : attribute_list* modifier* record_keyword (KW_CLASS | KW_STRUCT)? identifier_token type_parameter_list? parameter_list? base_list? type_parameter_constraint_clause* (LBRACE member_declaration* RBRACE)? OP_168?
  ;

struct_declaration
  : attribute_list* modifier* KW_STRUCT identifier_token type_parameter_list? parameter_list? base_list? type_parameter_constraint_clause* (LBRACE member_declaration* RBRACE)? OP_168?
  ;

union_declaration
  : attribute_list* modifier* KW_UNION identifier_token type_parameter_list? parameter_list? base_list? type_parameter_constraint_clause* (LBRACE member_declaration* RBRACE)? OP_168?
  ;

delegate_declaration
  : attribute_list* modifier* KW_DELEGATE type identifier_token type_parameter_list? parameter_list type_parameter_constraint_clause* OP_168
  ;

global_statement
  : attribute_list* modifier* statement
  ;

incomplete_member
  : attribute_list* modifier* type
  ;

type
  : array_type
  | function_pointer_type
  | name
  | nullable_type
  | pointer_type
  | predefined_type
  | ref_type
  | scoped_type
  | tuple_type
  ;

array_type
  : type array_rank_specifier+
  ;

array_rank_specifier
  : OP_173 (expression? (OP_163 expression?)*)? OP_174
  ;

function_pointer_type
  : KW_DELEGATE OP_161 function_pointer_calling_convention? function_pointer_parameter_list
  ;

function_pointer_calling_convention
  : KW_MANAGED function_pointer_unmanaged_calling_convention_list?
  | KW_UNMANAGED function_pointer_unmanaged_calling_convention_list?
  ;

function_pointer_unmanaged_calling_convention_list
  : OP_173 function_pointer_unmanaged_calling_convention (OP_163 function_pointer_unmanaged_calling_convention)* OP_174
  ;

function_pointer_unmanaged_calling_convention
  : identifier_token
  ;

function_pointer_parameter_list
  : OP_169 function_pointer_parameter (OP_163 function_pointer_parameter)* OP_171
  ;

function_pointer_parameter
  : attribute_list* modifier* type
  ;

nullable_type
  : type OP_172
  ;

pointer_type
  : type OP_161
  ;

predefined_type
  : KW_BOOL
  | KW_BYTE
  | KW_CHAR
  | KW_DECIMAL
  | KW_DOUBLE
  | KW_FLOAT
  | KW_INT
  | KW_LONG
  | KW_OBJECT
  | KW_SBYTE
  | KW_SHORT
  | KW_STRING
  | KW_UINT
  | KW_ULONG
  | KW_USHORT
  | KW_VOID
  ;

ref_type
  : KW_REF KW_READONLY? type
  ;

scoped_type
  : KW_SCOPED type
  ;

tuple_type
  : OP_159 tuple_element (OP_163 tuple_element)+ OP_160
  ;

tuple_element
  : type identifier_token?
  ;

statement
  : block
  | break_statement
  | checked_statement
  | common_for_each_statement
  | continue_statement
  | do_statement
  | empty_statement
  | expression_statement
  | fixed_statement
  | for_statement
  | goto_statement
  | if_statement
  | labeled_statement
  | local_declaration_statement
  | local_function_statement
  | lock_statement
  | return_statement
  | switch_statement
  | throw_statement
  | try_statement
  | unsafe_statement
  | using_statement
  | while_statement
  | yield_statement
  ;

break_statement
  : attribute_list* KW_BREAK identifier_name? OP_168
  ;

checked_statement
  : attribute_list* (KW_CHECKED | KW_UNCHECKED) block
  ;

common_for_each_statement
  : for_each_statement
  | for_each_variable_statement
  ;

for_each_statement
  : attribute_list* KW_AWAIT? KW_FOREACH OP_159 type identifier_token KW_IN expression OP_160 statement
  ;

for_each_variable_statement
  : attribute_list* KW_AWAIT? KW_FOREACH OP_159 expression KW_IN expression OP_160 statement
  ;

continue_statement
  : attribute_list* KW_CONTINUE identifier_name? OP_168
  ;

do_statement
  : attribute_list* KW_DO statement KW_WHILE OP_159 expression OP_160 OP_168
  ;

empty_statement
  : attribute_list* OP_168
  ;

expression_statement
  : attribute_list* expression OP_168
  ;

fixed_statement
  : attribute_list* KW_FIXED OP_159 variable_declaration OP_160 statement
  ;

for_statement
  : attribute_list* KW_FOR OP_159 (variable_declaration? | (expression (OP_163 expression)*)?) OP_168 expression? OP_168 (expression (OP_163 expression)*)? OP_160 statement
  ;

goto_statement
  : attribute_list* KW_GOTO (KW_CASE | KW_DEFAULT)? expression? OP_168
  ;

if_statement
  : attribute_list* KW_IF OP_159 expression OP_160 statement else_clause?
  ;

else_clause
  : KW_ELSE statement
  ;

labeled_statement
  : attribute_list* identifier_token COLON statement
  ;

local_declaration_statement
  : attribute_list* KW_AWAIT? KW_USING? modifier* variable_declaration OP_168
  ;

local_function_statement
  : attribute_list* modifier* type identifier_token type_parameter_list? parameter_list type_parameter_constraint_clause* (block | (arrow_expression_clause OP_168))
  ;

lock_statement
  : attribute_list* KW_LOCK OP_159 expression OP_160 statement
  ;

return_statement
  : attribute_list* KW_RETURN expression? OP_168
  ;

switch_statement
  : attribute_list* KW_SWITCH OP_159? expression OP_160? LBRACE switch_section* RBRACE
  ;

switch_section
  : switch_label+ statement+
  ;

switch_label
  : case_pattern_switch_label
  | case_switch_label
  | default_switch_label
  ;

case_pattern_switch_label
  : KW_CASE pattern when_clause? COLON
  ;

pattern
  : binary_pattern
  | constant_pattern
  | declaration_pattern
  | discard_pattern
  | list_pattern
  | parenthesized_pattern
  | recursive_pattern
  | relational_pattern
  | slice_pattern
  | type_pattern
  | unary_pattern
  | var_pattern
  ;

binary_pattern
  : pattern (KW_OR | KW_AND) pattern
  ;

constant_pattern
  : expression
  ;

declaration_pattern
  : type variable_designation
  ;

variable_designation
  : discard_designation
  | parenthesized_variable_designation
  | single_variable_designation
  ;

discard_designation
  : KW__
  ;

parenthesized_variable_designation
  : OP_159 (variable_designation (OP_163 variable_designation)*)? OP_160
  ;

single_variable_designation
  : identifier_token
  ;

discard_pattern
  : KW__
  ;

list_pattern
  : OP_173 (pattern (OP_163 pattern)* OP_163?)? OP_174 variable_designation?
  ;

parenthesized_pattern
  : OP_159 pattern OP_160
  ;

recursive_pattern
  : type? positional_pattern_clause? property_pattern_clause? variable_designation?
  ;

positional_pattern_clause
  : OP_159 (subpattern (OP_163 subpattern)*)? OP_160
  ;

subpattern
  : base_expression_colon? pattern
  ;

base_expression_colon
  : expression_colon
  | name_colon
  ;

expression_colon
  : expression COLON
  ;

property_pattern_clause
  : LBRACE (subpattern (OP_163 subpattern)* OP_163?)? RBRACE
  ;

relational_pattern
  : OP_117 expression
  | OP_169 expression
  | OP_133 expression
  | OP_134 expression
  | OP_171 expression
  | OP_136 expression
  ;

slice_pattern
  : OP_127 pattern?
  ;

type_pattern
  : type
  ;

unary_pattern
  : KW_NOT pattern
  ;

var_pattern
  : KW_VAR variable_designation
  ;

when_clause
  : KW_WHEN expression
  ;

case_switch_label
  : KW_CASE expression COLON
  ;

default_switch_label
  : KW_DEFAULT COLON
  ;

throw_statement
  : attribute_list* KW_THROW expression? OP_168
  ;

try_statement
  : attribute_list* KW_TRY block catch_clause* finally_clause?
  ;

catch_clause
  : KW_CATCH catch_declaration? catch_filter_clause? block
  ;

catch_declaration
  : OP_159 type identifier_token? OP_160
  ;

catch_filter_clause
  : KW_WHEN OP_159 expression OP_160
  ;

finally_clause
  : KW_FINALLY block
  ;

unsafe_statement
  : attribute_list* KW_UNSAFE block
  ;

using_statement
  : attribute_list* KW_AWAIT? KW_USING OP_159 (variable_declaration | expression) OP_160 statement
  ;

while_statement
  : attribute_list* KW_WHILE OP_159 expression OP_160 statement
  ;

yield_statement
  : attribute_list* KW_YIELD (KW_RETURN | KW_BREAK) expression? OP_168
  ;

expression
  : anonymous_function_expression
  | anonymous_object_creation_expression
  | array_creation_expression
  | assignment_expression
  | await_expression
  | base_object_creation_expression
  | binary_expression
  | cast_expression
  | checked_expression
  | collection_expression
  | conditional_access_expression
  | conditional_expression
  | declaration_expression
  | default_expression
  | element_access_expression
  | element_binding_expression
  | field_expression
  | implicit_array_creation_expression
  | implicit_element_access
  | implicit_stack_alloc_array_creation_expression
  | initializer_expression
  | instance_expression
  | interpolated_string_expression
  | invocation_expression
  | is_pattern_expression
  | literal_expression
  | make_ref_expression
  | member_access_expression
  | member_binding_expression
  | parenthesized_expression
  | postfix_unary_expression
  | prefix_unary_expression
  | query_expression
  | range_expression
  | ref_expression
  | ref_type_expression
  | ref_value_expression
  | size_of_expression
  | stack_alloc_array_creation_expression
  | switch_expression
  | throw_expression
  | tuple_expression
  | type
  | type_of_expression
  | unsafe_expression
  | with_expression
  ;

anonymous_function_expression
  : anonymous_method_expression
  | lambda_expression
  ;

anonymous_method_expression
  : modifier* KW_DELEGATE parameter_list? block expression?
  ;

lambda_expression
  : parenthesized_lambda_expression
  | simple_lambda_expression
  ;

parenthesized_lambda_expression
  : attribute_list* modifier* type? parameter_list OP_135 (block | expression)
  ;

simple_lambda_expression
  : attribute_list* modifier* parameter OP_135 (block | expression)
  ;

anonymous_object_creation_expression
  : KW_NEW LBRACE (anonymous_object_member_declarator (OP_163 anonymous_object_member_declarator)* OP_163?)? RBRACE
  ;

anonymous_object_member_declarator
  : name_equals? expression
  ;

array_creation_expression
  : KW_NEW array_type initializer_expression?
  ;

initializer_expression
  : LBRACE (expression (OP_163 expression)* OP_163?)? RBRACE
  ;

assignment_expression
  : expression (OP_170 | OP_123 | OP_125 | OP_121 | OP_128 | OP_118 | OP_120 | OP_141 | OP_151 | OP_102 | right_shift_assignment | unsigned_right_shift_assignment | OP_103) expression
  ;

await_expression
  : KW_AWAIT expression
  ;

base_object_creation_expression
  : implicit_object_creation_expression
  | object_creation_expression
  ;

implicit_object_creation_expression
  : KW_NEW argument_list initializer_expression?
  ;

object_creation_expression
  : KW_NEW type argument_list? initializer_expression?
  ;

binary_expression
  : expression (OP_162 | OP_164 | OP_161 | OP_166 | OP_157 | OP_132 | right_shift | unsigned_right_shift | OP_152 | OP_119 | OP_178 | OP_158 | OP_175 | OP_134 | OP_117 | OP_169 | OP_133 | OP_171 | OP_136 | KW_IS | KW_AS | OP_137) expression
  ;

cast_expression
  : OP_159 type OP_160 expression
  ;

checked_expression
  : KW_CHECKED OP_159 expression OP_160
  | KW_UNCHECKED OP_159 expression OP_160
  ;

collection_expression
  : OP_173 (collection_element (OP_163 collection_element)* OP_163?)? OP_174
  ;

collection_element
  : expression_element
  | spread_element
  | with_element
  ;

expression_element
  : expression
  ;

spread_element
  : OP_127 expression
  ;

with_element
  : KW_WITH argument_list
  ;

conditional_access_expression
  : expression OP_172 expression
  ;

conditional_expression
  : expression OP_172 expression COLON expression
  ;

declaration_expression
  : type variable_designation
  ;

default_expression
  : KW_DEFAULT OP_159 type OP_160
  ;

element_access_expression
  : expression bracketed_argument_list
  ;

element_binding_expression
  : bracketed_argument_list
  ;

field_expression
  : KW_FIELD
  ;

implicit_array_creation_expression
  : KW_NEW OP_173 OP_163* OP_174 initializer_expression
  ;

implicit_element_access
  : bracketed_argument_list
  ;

implicit_stack_alloc_array_creation_expression
  : KW_STACKALLOC OP_173 OP_174 initializer_expression
  ;

instance_expression
  : base_expression
  | this_expression
  ;

base_expression
  : KW_BASE
  ;

this_expression
  : KW_THIS
  ;

interpolated_string_expression
  : INTERP_START interpolated_string_content* DQUOTE
  | INTERP_VERBATIM_START interpolated_string_content* DQUOTE
  | interpolated_multi_line_raw_string_start_token interpolated_string_content* interpolated_raw_string_end_token
  | interpolated_single_line_raw_string_start_token interpolated_string_content* interpolated_raw_string_end_token
  ;

interpolated_string_content
  : interpolated_string_text
  | interpolation
  ;

interpolated_string_text
  : interpolated_string_text_token
  ;

interpolation
  : LBRACE expression interpolation_alignment_clause? interpolation_format_clause? RBRACE
  ;

interpolation_alignment_clause
  : OP_163 expression
  ;

interpolation_format_clause
  : COLON interpolated_string_text_token
  ;

interpolated_multi_line_raw_string_start_token
  : OP_156+ OP_101 DQUOTE*
  ;

interpolated_raw_string_end_token
  : OP_101 DQUOTE* /* must match number of quotes in raw_string_start_token */
  ;

interpolated_single_line_raw_string_start_token
  : OP_156+ OP_101 DQUOTE*
  ;

invocation_expression
  : expression argument_list
  ;

is_pattern_expression
  : expression KW_IS pattern
  ;

literal_expression
  : KW_DEFAULT
  | KW_FALSE
  | KW_NULL
  | KW_TRUE
  | KW___ARGLIST
  | character_literal_token
  | multi_line_raw_string_literal_token
  | numeric_literal_token
  | single_line_raw_string_literal_token
  | string_literal_token
  | utf8_multi_line_raw_string_literal_token
  | utf8_single_line_raw_string_literal_token
  | utf8_string_literal_token
  ;

utf8_multi_line_raw_string_literal_token
  : multi_line_raw_string_literal_token (KW_U8 | KW_U8_150)
  ;

utf8_single_line_raw_string_literal_token
  : single_line_raw_string_literal_token (KW_U8 | KW_U8_150)
  ;

utf8_string_literal_token
  : string_literal_token (KW_U8 | KW_U8_150)
  ;

make_ref_expression
  : KW___MAKEREF OP_159 expression OP_160
  ;

member_access_expression
  : expression (OP_165 | OP_126) simple_name
  ;

member_binding_expression
  : OP_165 simple_name
  ;

parenthesized_expression
  : OP_159 expression OP_160
  ;

postfix_unary_expression
  : expression (OP_122 | OP_124 | OP_153)
  ;

prefix_unary_expression
  : OP_153 expression
  | OP_158 expression
  | OP_161 expression
  | OP_162 expression
  | OP_122 expression
  | OP_164 expression
  | OP_124 expression
  | OP_175 expression
  | OP_180 expression
  ;

query_expression
  : from_clause query_body
  ;

from_clause
  : KW_FROM type? identifier_token KW_IN expression
  ;

query_body
  : query_clause+ select_or_group_clause query_continuation?
  ;

query_clause
  : from_clause
  | join_clause
  | let_clause
  | order_by_clause
  | where_clause
  ;

join_clause
  : KW_JOIN type? identifier_token KW_IN expression KW_ON expression KW_EQUALS expression join_into_clause?
  ;

join_into_clause
  : KW_INTO identifier_token
  ;

let_clause
  : KW_LET identifier_token OP_170 expression
  ;

order_by_clause
  : KW_ORDERBY ordering (OP_163 ordering)*
  ;

ordering
  : expression (KW_ASCENDING | KW_DESCENDING)?
  ;

where_clause
  : KW_WHERE expression
  ;

select_or_group_clause
  : group_clause
  | select_clause
  ;

group_clause
  : KW_GROUP expression KW_BY expression
  ;

select_clause
  : KW_SELECT expression
  ;

query_continuation
  : KW_INTO identifier_token query_body
  ;

range_expression
  : expression? OP_127 expression?
  ;

ref_expression
  : KW_REF expression
  ;

ref_type_expression
  : KW___REFTYPE OP_159 expression OP_160
  ;

ref_value_expression
  : KW___REFVALUE OP_159 expression OP_163 type OP_160
  ;

size_of_expression
  : KW_SIZEOF OP_159 type OP_160
  ;

stack_alloc_array_creation_expression
  : KW_STACKALLOC type initializer_expression?
  ;

switch_expression
  : expression KW_SWITCH LBRACE (switch_expression_arm (OP_163 switch_expression_arm)* OP_163?)? RBRACE
  ;

switch_expression_arm
  : pattern when_clause? OP_135 expression
  ;

throw_expression
  : KW_THROW expression
  ;

tuple_expression
  : OP_159 argument (OP_163 argument)+ OP_160
  ;

type_of_expression
  : KW_TYPEOF OP_159 type OP_160
  ;

unsafe_expression
  : KW_UNSAFE OP_159 expression OP_160
  ;

with_expression
  : expression KW_WITH initializer_expression
  ;
























































syntax_token
  : character_literal_token
  | identifier_token
  | keyword
  | numeric_literal_token
  | operator_token
  | punctuation_token
  | string_literal_token
  ;

// A C# *contextual* keyword is recognized only in the position where it has
// meaning and is otherwise an ordinary name: `var`, `record`, `from`, `get`,
// `and`, `required`, … Roslyn's grammar spells each as an inline literal, and
// harvesting those literals into named tokens makes the lexer prefer the keyword
// everywhere (ANTLR breaks an equal-length match by rule order, and the
// harvested tokens precede IDENTIFIER). Widening `identifier_token` to accept
// them back is the standard ANTLR remedy — the same shape `grammars-v4`'s C#
// grammar uses for its `identifier` rule.
//
// Without this, `var x = 1;` fails to parse: `var` is absent from the expected
// token set entirely. The damage is far wider than the keyword itself, because
// `var` appears in most idiomatic modern C# — one wrong token classification
// looks like broken support for raw strings, ranges, `using` declarations, and
// unbound generics all at once.
identifier_token
  : IDENTIFIER
  | KW__
  | KW_ADD
  | KW_ALIAS
  | KW_ALLOWS
  | KW_AND
  | KW_ASCENDING
  | KW_ASYNC
  | KW_AWAIT
  | KW_BY
  | KW_CLOSED
  | KW_DESCENDING
  | KW_EQUALS
  | KW_EXTENSION
  | KW_FIELD
  | KW_FILE
  | KW_FROM
  | KW_GET
  | KW_GLOBAL
  | KW_GROUP
  | KW_INIT
  | KW_INTO
  | KW_JOIN
  | KW_LET
  | KW_MANAGED
  | KW_NOT
  | KW_ON
  | KW_OR
  | KW_ORDERBY
  | KW_PARTIAL
  | KW_REMOVE
  | KW_REQUIRED
  | KW_SAFE
  | KW_SCOPED
  | KW_SELECT
  | KW_SET
  | KW_UNION
  | KW_UNMANAGED
  | KW_VAR
  | KW_WHEN
  | KW_WHERE
  | KW_WITH
  | KW_YIELD
  ;









keyword
  : KW_AS
  | KW_BASE
  | KW_BOOL
  | KW_BREAK
  | KW_BYTE
  | KW_CASE
  | KW_CATCH
  | KW_CHAR
  | KW_CHECKED
  | KW_CLASS
  | KW_CONTINUE
  | KW_DECIMAL
  | KW_DEFAULT
  | KW_DELEGATE
  | KW_DO
  | KW_DOUBLE
  | KW_ELSE
  | KW_ENUM
  | KW_EVENT
  | KW_EXPLICIT
  | KW_FALSE
  | KW_FINALLY
  | KW_FLOAT
  | KW_FOR
  | KW_FOREACH
  | KW_GOTO
  | KW_IF
  | KW_IMPLICIT
  | KW_IN
  | KW_INT
  | KW_INTERFACE
  | KW_IS
  | KW_LOCK
  | KW_LONG
  | KW_NAMESPACE
  | KW_NULL
  | KW_OBJECT
  | KW_OPERATOR
  | KW_OUT
  | KW_PARAMS
  | KW_RETURN
  | KW_SBYTE
  | KW_SHORT
  | KW_SIZEOF
  | KW_STACKALLOC
  | KW_STRING
  | KW_STRUCT
  | KW_SWITCH
  | KW_THIS
  | KW_THROW
  | KW_TRUE
  | KW_TRY
  | KW_TYPEOF
  | KW_UINT
  | KW_ULONG
  | KW_UNCHECKED
  | KW_USHORT
  | KW_USING
  | KW_VOID
  | KW_WHILE
  | KW___ARGLIST
  | KW___MAKEREF
  | KW___REFTYPE
  | KW___REFVALUE
  | modifier
  ;

numeric_literal_token
  : integer_literal_token
  | real_literal_token
  ;

integer_literal_token
  : decimal_integer_literal_token
  | hexadecimal_integer_literal_token
  | BIN_INT_LIT
  ;

decimal_integer_literal_token
  : DEC_INT_LIT
  ;



hexadecimal_integer_literal_token
  : HEX_INT_LIT
  ;


real_literal_token
  : REAL_LIT
  ;



character_literal_token
  : CHAR_LIT
  ;






string_literal_token
  : regular_string_literal_token
  | verbatim_string_literal_token
  ;

regular_string_literal_token
  : STRING_LIT
  ;



verbatim_string_literal_token
  : VERBATIM_STRING_LIT
  ;




operator_token
  : OP_153
  | OP_117
  | OP_157
  | OP_118
  | OP_119
  | OP_158
  | OP_120
  | OP_161
  | OP_121
  | OP_162
  | OP_122
  | OP_123
  | OP_164
  | OP_124
  | OP_125
  | OP_166
  | OP_128
  | OP_169
  | OP_132
  | OP_102
  | OP_133
  | OP_170
  | OP_134
  | OP_171
  | OP_136
  | right_shift
  | right_shift_assignment
  | unsigned_right_shift
  | unsigned_right_shift_assignment
  | OP_137
  | OP_103
  | KW_AS
  | KW_IS
  | OP_175
  | OP_141
  | OP_178
  | OP_151
  | OP_152
  | OP_180
  ;

punctuation_token
  : DQUOTE
  | OP_155
  | OP_159
  | OP_160
  | OP_163
  | OP_126
  | OP_165
  | OP_127
  | OP_129
  | COLON
  | OP_130
  | OP_168
  | OP_131
  | OP_135
  | OP_172
  | OP_173
  | OP_139
  | OP_140
  | OP_174
  | LBRACE
  | RBRACE
  ;






interpolated_string_text_token
  : INTERPOLATED_TEXT
  ;

multi_line_raw_string_literal_token
  : ML_RAW_STRING_LIT
  ;

single_line_raw_string_literal_token
  : SL_RAW_STRING_LIT
  ;



// Contextual keyword: `record` lexes as an ordinary IDENTIFIER (it is legal as a
// name), so the declaration position is restricted by a predicate on the token
// text. This restores Roslyn's <ContextualKind Name="RecordKeyword"/>, which its
// grammar generator drops. Lowered by `patterns.toml` to a pure SemIR
// comparison, so no hooks are needed.
//
// Deliberately `IDENTIFIER`, not `identifier_token`: the latter is widened to
// accept every contextual keyword (see CONTEXTUAL_KEYWORD_NOTE), which would
// make `record_declaration` viable at any modifier that is itself contextual.
// `partial struct S { }` would then predict the record path — `record_keyword` =
// `partial`, `struct`, `S` — and since the predicate cannot prune a path ANTLR
// has already committed to, it surfaces as a hard error instead of a silent
// rejection. `partial`, `async`, `required`, `file`, `scoped`, `closed`, and
// `safe` all hit this. `record` itself always lexes as IDENTIFIER, so nothing is
// lost.
record_keyword
  : {this.IsRecordKeyword()}? IDENTIFIER
  ;

right_shift
  : OP_171 OP_171 {this.IsRightShift()}? // adjacent in the char stream?
  ;

unsigned_right_shift
  : OP_171 OP_171 {this.IsUnsignedRightShift()}? OP_171 {this.IsUnsignedRightShift()}? // adjacent in the char stream?
  ;

right_shift_assignment
  : OP_171 OP_136 {this.IsRightShiftAssignment()}? // adjacent in the char stream?
  ;

unsigned_right_shift_assignment
  : OP_171 OP_171 {this.IsUnsignedRightShiftAssignment()}? OP_136 {this.IsUnsignedRightShiftAssignment()}? // adjacent in the char stream?
  ;
