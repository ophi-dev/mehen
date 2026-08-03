// @generated from Roslyn's CSharp.Generated.g4 by prepare-roslyn-grammar.py — do not hand-edit.
// See PROVENANCE.md for the pinned upstream revision and the patch rationale.
parser grammar CSharpParser;

options { tokenVocab=CSharpLexer; }

compilation_unit
  : extern_alias_directive* using_directive* attribute_list* member_declaration*
  ;

extern_alias_directive
  : KW_EXTERN KW_ALIAS identifier_token OP_172
  ;

using_directive
  : KW_GLOBAL? KW_USING (KW_STATIC | (KW_UNSAFE? name_equals))? type OP_172
  ;

name_equals
  : identifier_name OP_174
  ;

identifier_name
  : KW_GLOBAL
  | identifier_token
  ;

attribute_list
  : OP_177 attribute_target_specifier? attribute (OP_167 attribute)* OP_178
  ;

attribute_target_specifier
  : syntax_token OP_171
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
  : identifier_name OP_133 simple_name
  ;

simple_name
  : generic_name
  | identifier_name
  ;

generic_name
  : identifier_token type_argument_list
  ;

type_argument_list
  : OP_173 (type? (OP_167 type?)*)? OP_175
  ;

qualified_name
  : name OP_169 simple_name
  ;

attribute_argument_list
  : OP_163 (attribute_argument (OP_167 attribute_argument)*)? OP_164
  ;

attribute_argument
  : (name_equals? | name_colon?) expression
  ;

name_colon
  : identifier_name OP_171
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
  : attribute_list* modifier* KW_EVENT variable_declaration OP_172
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
  : type variable_declarator (OP_167 variable_declarator)*
  ;

variable_declarator
  : identifier_token bracketed_argument_list? equals_value_clause?
  ;

bracketed_argument_list
  : OP_177 argument (OP_167 argument)* OP_178
  ;

argument
  : name_colon? (KW_REF | KW_OUT | KW_IN)? expression
  ;

equals_value_clause
  : OP_174 expression
  ;

field_declaration
  : attribute_list* modifier* variable_declaration OP_172
  ;

base_method_declaration
  : constructor_declaration
  | conversion_operator_declaration
  | destructor_declaration
  | method_declaration
  | operator_declaration
  ;

constructor_declaration
  : attribute_list* modifier* identifier_token parameter_list constructor_initializer? (block | (arrow_expression_clause OP_172))
  ;

parameter_list
  : OP_163 (parameter (OP_167 parameter)*)? OP_164
  ;

parameter
  : attribute_list* modifier* type? (identifier_token | KW___ARGLIST)? equals_value_clause?
  ;

constructor_initializer
  : OP_171 (KW_BASE | KW_THIS) argument_list
  ;

argument_list
  : OP_163 (argument (OP_167 argument)*)? OP_164
  ;

block
  : attribute_list* OP_181 statement* OP_183
  ;

arrow_expression_clause
  : OP_138 expression
  ;

conversion_operator_declaration
  : attribute_list* modifier* (KW_IMPLICIT | KW_EXPLICIT) explicit_interface_specifier? KW_OPERATOR KW_CHECKED? type parameter_list (block | (arrow_expression_clause OP_172))
  ;

explicit_interface_specifier
  : name OP_169
  ;

destructor_declaration
  : attribute_list* modifier* OP_184 identifier_token parameter_list (block | (arrow_expression_clause OP_172))
  ;

method_declaration
  : attribute_list* modifier* type explicit_interface_specifier? identifier_token type_parameter_list? parameter_list type_parameter_constraint_clause* (block | (arrow_expression_clause OP_172))
  ;

type_parameter_list
  : OP_173 type_parameter (OP_167 type_parameter)* OP_175
  ;

type_parameter
  : attribute_list* (KW_IN | KW_OUT)? identifier_token
  ;

type_parameter_constraint_clause
  : KW_WHERE identifier_name OP_171 type_parameter_constraint (OP_167 type_parameter_constraint)*
  ;

type_parameter_constraint
  : allows_constraint_clause
  | class_or_struct_constraint
  | constructor_constraint
  | default_constraint
  | type_constraint
  ;

allows_constraint_clause
  : KW_ALLOWS allows_constraint (OP_167 allows_constraint)*
  ;

allows_constraint
  : ref_struct_constraint
  ;

ref_struct_constraint
  : KW_REF KW_STRUCT
  ;

class_or_struct_constraint
  : KW_CLASS OP_176?
  | KW_STRUCT OP_176?
  ;

constructor_constraint
  : KW_NEW OP_163 OP_164
  ;

default_constraint
  : KW_DEFAULT
  ;

type_constraint
  : type
  ;

operator_declaration
  : attribute_list* modifier* type explicit_interface_specifier? KW_OPERATOR KW_CHECKED? (OP_166 | OP_168 | OP_157 | OP_184 | OP_125 | OP_127 | OP_165 | OP_170 | OP_161 | OP_135 | OP_140 | OP_105 | OP_182 | OP_162 | OP_179 | OP_137 | OP_120 | OP_173 | OP_136 | OP_175 | OP_139 | KW_FALSE | KW_TRUE | KW_IS | OP_126 | OP_128 | OP_124 | OP_131 | OP_121 | OP_123 | OP_155 | OP_145 | OP_103 | OP_104 | OP_078) parameter_list (block | (arrow_expression_clause OP_172))
  ;

base_namespace_declaration
  : file_scoped_namespace_declaration
  | namespace_declaration
  ;

file_scoped_namespace_declaration
  : attribute_list* modifier* KW_NAMESPACE name OP_172 extern_alias_directive* using_directive* member_declaration*
  ;

namespace_declaration
  : attribute_list* modifier* KW_NAMESPACE name OP_181 extern_alias_directive* using_directive* member_declaration* OP_183 OP_172?
  ;

base_property_declaration
  : event_declaration
  | indexer_declaration
  | property_declaration
  ;

event_declaration
  : attribute_list* modifier* KW_EVENT type explicit_interface_specifier? identifier_token (accessor_list | OP_172)
  ;

accessor_list
  : OP_181 accessor_declaration* OP_183
  ;

accessor_declaration
  : attribute_list* modifier* (KW_GET | KW_SET | KW_INIT | KW_ADD | KW_REMOVE | identifier_token) (block | (arrow_expression_clause OP_172))
  ;

indexer_declaration
  : attribute_list* modifier* type explicit_interface_specifier? KW_THIS bracketed_parameter_list (accessor_list | (arrow_expression_clause OP_172))
  ;

bracketed_parameter_list
  : OP_177 parameter (OP_167 parameter)* OP_178
  ;

property_declaration
  : attribute_list* modifier* type explicit_interface_specifier? identifier_token (accessor_list | ((arrow_expression_clause | equals_value_clause) OP_172))
  ;

base_type_declaration
  : enum_declaration
  | type_declaration
  ;

enum_declaration
  : attribute_list* modifier* KW_ENUM identifier_token base_list? OP_181? (enum_member_declaration (OP_167 enum_member_declaration)* OP_167?)? OP_183? OP_172?
  ;

base_list
  : OP_171 base_type (OP_167 base_type)*
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
  : attribute_list* modifier* KW_CLASS identifier_token type_parameter_list? parameter_list? base_list? type_parameter_constraint_clause* OP_181? member_declaration* OP_183? OP_172?
  ;

extension_block_declaration
  : attribute_list* modifier* KW_EXTENSION type_parameter_list? parameter_list? type_parameter_constraint_clause* OP_181? member_declaration* OP_183? OP_172?
  ;

interface_declaration
  : attribute_list* modifier* KW_INTERFACE identifier_token type_parameter_list? parameter_list? base_list? type_parameter_constraint_clause* OP_181? member_declaration* OP_183? OP_172?
  ;

record_declaration
  : attribute_list* modifier* syntax_token (KW_CLASS | KW_STRUCT)? identifier_token type_parameter_list? parameter_list? base_list? type_parameter_constraint_clause* OP_181? member_declaration* OP_183? OP_172?
  ;

struct_declaration
  : attribute_list* modifier* KW_STRUCT identifier_token type_parameter_list? parameter_list? base_list? type_parameter_constraint_clause* OP_181? member_declaration* OP_183? OP_172?
  ;

union_declaration
  : attribute_list* modifier* KW_UNION identifier_token type_parameter_list? parameter_list? base_list? type_parameter_constraint_clause* OP_181? member_declaration* OP_183? OP_172?
  ;

delegate_declaration
  : attribute_list* modifier* KW_DELEGATE type identifier_token type_parameter_list? parameter_list type_parameter_constraint_clause* OP_172
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
  : OP_177 (expression? (OP_167 expression?)*)? OP_178
  ;

function_pointer_type
  : KW_DELEGATE OP_165 function_pointer_calling_convention? function_pointer_parameter_list
  ;

function_pointer_calling_convention
  : KW_MANAGED function_pointer_unmanaged_calling_convention_list?
  | KW_UNMANAGED function_pointer_unmanaged_calling_convention_list?
  ;

function_pointer_unmanaged_calling_convention_list
  : OP_177 function_pointer_unmanaged_calling_convention (OP_167 function_pointer_unmanaged_calling_convention)* OP_178
  ;

function_pointer_unmanaged_calling_convention
  : identifier_token
  ;

function_pointer_parameter_list
  : OP_173 function_pointer_parameter (OP_167 function_pointer_parameter)* OP_175
  ;

function_pointer_parameter
  : attribute_list* modifier* type
  ;

nullable_type
  : type OP_176
  ;

pointer_type
  : type OP_165
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
  : OP_163 tuple_element (OP_167 tuple_element)+ OP_164
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
  : attribute_list* KW_BREAK identifier_name? OP_172
  ;

checked_statement
  : attribute_list* (KW_CHECKED | KW_UNCHECKED) block
  ;

common_for_each_statement
  : for_each_statement
  | for_each_variable_statement
  ;

for_each_statement
  : attribute_list* KW_AWAIT? KW_FOREACH OP_163 type identifier_token KW_IN expression OP_164 statement
  ;

for_each_variable_statement
  : attribute_list* KW_AWAIT? KW_FOREACH OP_163 expression KW_IN expression OP_164 statement
  ;

continue_statement
  : attribute_list* KW_CONTINUE identifier_name? OP_172
  ;

do_statement
  : attribute_list* KW_DO statement KW_WHILE OP_163 expression OP_164 OP_172
  ;

empty_statement
  : attribute_list* OP_172
  ;

expression_statement
  : attribute_list* expression OP_172
  ;

fixed_statement
  : attribute_list* KW_FIXED OP_163 variable_declaration OP_164 statement
  ;

for_statement
  : attribute_list* KW_FOR OP_163 (variable_declaration? | (expression (OP_167 expression)*)?) OP_172 expression? OP_172 (expression (OP_167 expression)*)? OP_164 statement
  ;

goto_statement
  : attribute_list* KW_GOTO (KW_CASE | KW_DEFAULT)? expression? OP_172
  ;

if_statement
  : attribute_list* KW_IF OP_163 expression OP_164 statement else_clause?
  ;

else_clause
  : KW_ELSE statement
  ;

labeled_statement
  : attribute_list* identifier_token OP_171 statement
  ;

local_declaration_statement
  : attribute_list* KW_AWAIT? KW_USING? modifier* variable_declaration OP_172
  ;

local_function_statement
  : attribute_list* modifier* type identifier_token type_parameter_list? parameter_list type_parameter_constraint_clause* (block | (arrow_expression_clause OP_172))
  ;

lock_statement
  : attribute_list* KW_LOCK OP_163 expression OP_164 statement
  ;

return_statement
  : attribute_list* KW_RETURN expression? OP_172
  ;

switch_statement
  : attribute_list* KW_SWITCH OP_163? expression OP_164? OP_181 switch_section* OP_183
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
  : KW_CASE pattern when_clause? OP_171
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
  : OP_163 (variable_designation (OP_167 variable_designation)*)? OP_164
  ;

single_variable_designation
  : identifier_token
  ;

discard_pattern
  : KW__
  ;

list_pattern
  : OP_177 (pattern (OP_167 pattern)* OP_167?)? OP_178 variable_designation?
  ;

parenthesized_pattern
  : OP_163 pattern OP_164
  ;

recursive_pattern
  : type? positional_pattern_clause? property_pattern_clause? variable_designation?
  ;

positional_pattern_clause
  : OP_163 (subpattern (OP_167 subpattern)*)? OP_164
  ;

subpattern
  : base_expression_colon? pattern
  ;

base_expression_colon
  : expression_colon
  | name_colon
  ;

expression_colon
  : expression OP_171
  ;

property_pattern_clause
  : OP_181 (subpattern (OP_167 subpattern)* OP_167?)? OP_183
  ;

relational_pattern
  : OP_120 expression
  | OP_173 expression
  | OP_136 expression
  | OP_137 expression
  | OP_175 expression
  | OP_139 expression
  ;

slice_pattern
  : OP_130 pattern?
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
  : KW_CASE expression OP_171
  ;

default_switch_label
  : KW_DEFAULT OP_171
  ;

throw_statement
  : attribute_list* KW_THROW expression? OP_172
  ;

try_statement
  : attribute_list* KW_TRY block catch_clause* finally_clause?
  ;

catch_clause
  : KW_CATCH catch_declaration? catch_filter_clause? block
  ;

catch_declaration
  : OP_163 type identifier_token? OP_164
  ;

catch_filter_clause
  : KW_WHEN OP_163 expression OP_164
  ;

finally_clause
  : KW_FINALLY block
  ;

unsafe_statement
  : attribute_list* KW_UNSAFE block
  ;

using_statement
  : attribute_list* KW_AWAIT? KW_USING OP_163 (variable_declaration | expression) OP_164 statement
  ;

while_statement
  : attribute_list* KW_WHILE OP_163 expression OP_164 statement
  ;

yield_statement
  : attribute_list* KW_YIELD (KW_RETURN | KW_BREAK) expression? OP_172
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
  : attribute_list* modifier* type? parameter_list OP_138 (block | expression)
  ;

simple_lambda_expression
  : attribute_list* modifier* parameter OP_138 (block | expression)
  ;

anonymous_object_creation_expression
  : KW_NEW OP_181 (anonymous_object_member_declarator (OP_167 anonymous_object_member_declarator)* OP_167?)? OP_183
  ;

anonymous_object_member_declarator
  : name_equals? expression
  ;

array_creation_expression
  : KW_NEW array_type initializer_expression?
  ;

initializer_expression
  : OP_181 (expression (OP_167 expression)* OP_167?)? OP_183
  ;

assignment_expression
  : expression (OP_174 | OP_126 | OP_128 | OP_124 | OP_131 | OP_121 | OP_123 | OP_145 | OP_155 | OP_103 | OP_104 | OP_078 | OP_106) expression
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
  : expression (OP_166 | OP_168 | OP_165 | OP_170 | OP_161 | OP_135 | OP_140 | OP_105 | OP_156 | OP_122 | OP_182 | OP_162 | OP_179 | OP_137 | OP_120 | OP_173 | OP_136 | OP_175 | OP_139 | KW_IS | KW_AS | OP_141) expression
  ;

cast_expression
  : OP_163 type OP_164 expression
  ;

checked_expression
  : KW_CHECKED OP_163 expression OP_164
  | KW_UNCHECKED OP_163 expression OP_164
  ;

collection_expression
  : OP_177 (collection_element (OP_167 collection_element)* OP_167?)? OP_178
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
  : OP_130 expression
  ;

with_element
  : KW_WITH argument_list
  ;

conditional_access_expression
  : expression OP_176 expression
  ;

conditional_expression
  : expression OP_176 expression OP_171 expression
  ;

declaration_expression
  : type variable_designation
  ;

default_expression
  : KW_DEFAULT OP_163 type OP_164
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
  : KW_NEW OP_177 OP_167* OP_178 initializer_expression
  ;

implicit_element_access
  : bracketed_argument_list
  ;

implicit_stack_alloc_array_creation_expression
  : KW_STACKALLOC OP_177 OP_178 initializer_expression
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
  : INTERP_START interpolated_string_content* OP_158
  | INTERP_VERBATIM_START interpolated_string_content* OP_158
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
  : OP_181 expression interpolation_alignment_clause? interpolation_format_clause? OP_183
  ;

interpolation_alignment_clause
  : OP_167 expression
  ;

interpolation_format_clause
  : OP_171 interpolated_string_text_token
  ;

interpolated_multi_line_raw_string_start_token
  : OP_160+ OP_102 OP_158*
  ;

interpolated_raw_string_end_token
  : OP_102 OP_158* /* must match number of quotes in raw_string_start_token */
  ;

interpolated_single_line_raw_string_start_token
  : OP_160+ OP_102 OP_158*
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
  : multi_line_raw_string_literal_token (KW_U8 | KW_U8_154)
  ;

utf8_single_line_raw_string_literal_token
  : single_line_raw_string_literal_token (KW_U8 | KW_U8_154)
  ;

utf8_string_literal_token
  : string_literal_token (KW_U8 | KW_U8_154)
  ;

make_ref_expression
  : KW___MAKEREF OP_163 expression OP_164
  ;

member_access_expression
  : expression (OP_169 | OP_129) simple_name
  ;

member_binding_expression
  : OP_169 simple_name
  ;

parenthesized_expression
  : OP_163 expression OP_164
  ;

postfix_unary_expression
  : expression (OP_125 | OP_127 | OP_157)
  ;

prefix_unary_expression
  : OP_157 expression
  | OP_162 expression
  | OP_165 expression
  | OP_166 expression
  | OP_125 expression
  | OP_168 expression
  | OP_127 expression
  | OP_179 expression
  | OP_184 expression
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
  : KW_LET identifier_token OP_174 expression
  ;

order_by_clause
  : KW_ORDERBY ordering (OP_167 ordering)*
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
  : expression? OP_130 expression?
  ;

ref_expression
  : KW_REF expression
  ;

ref_type_expression
  : KW___REFTYPE OP_163 expression OP_164
  ;

ref_value_expression
  : KW___REFVALUE OP_163 expression OP_167 type OP_164
  ;

size_of_expression
  : KW_SIZEOF OP_163 type OP_164
  ;

stack_alloc_array_creation_expression
  : KW_STACKALLOC type initializer_expression?
  ;

switch_expression
  : expression KW_SWITCH OP_181 (switch_expression_arm (OP_167 switch_expression_arm)* OP_167?)? OP_183
  ;

switch_expression_arm
  : pattern when_clause? OP_138 expression
  ;

throw_expression
  : KW_THROW expression
  ;

tuple_expression
  : OP_163 argument (OP_167 argument)+ OP_164
  ;

type_of_expression
  : KW_TYPEOF OP_163 type OP_164
  ;

unsafe_expression
  : KW_UNSAFE OP_163 expression OP_164
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

identifier_token
  : IDENTIFIER
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
  : OP_157
  | OP_120
  | OP_161
  | OP_121
  | OP_122
  | OP_162
  | OP_123
  | OP_165
  | OP_124
  | OP_166
  | OP_125
  | OP_126
  | OP_168
  | OP_127
  | OP_128
  | OP_170
  | OP_131
  | OP_173
  | OP_135
  | OP_103
  | OP_136
  | OP_174
  | OP_137
  | OP_175
  | OP_139
  | OP_140
  | OP_104
  | OP_105
  | OP_078
  | OP_141
  | OP_106
  | KW_AS
  | KW_IS
  | OP_179
  | OP_145
  | OP_182
  | OP_155
  | OP_156
  | OP_184
  ;

punctuation_token
  : OP_158
  | OP_159
  | OP_163
  | OP_164
  | OP_167
  | OP_129
  | OP_169
  | OP_130
  | OP_132
  | OP_171
  | OP_133
  | OP_172
  | OP_134
  | OP_138
  | OP_176
  | OP_177
  | OP_143
  | OP_144
  | OP_178
  | OP_181
  | OP_183
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
// UNREFERENCED IN THIS VARIANT. This is the `slow` control: `record_declaration`
// above deliberately keeps Roslyn's catch-all `syntax_token`, which is what makes
// `class` viable as a record and drives the timing blow-up being reproduced. The
// rule (and the `IsRecordKeyword` helper in this directory's `patterns.toml`) is
// kept only so both variants generate from the identical helper set, isolating the
// single grammar difference under measurement.
record_keyword
  : {this.IsRecordKeyword()}? identifier_token
  ;
