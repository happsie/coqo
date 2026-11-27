; TypeScript and TSX definitions (tags conventions: @definition.<kind> and
; @name).

(function_declaration name: (identifier) @name) @definition.function
(generator_function_declaration name: (identifier) @name) @definition.function
(function_signature name: (identifier) @name) @definition.function
(class_declaration name: (type_identifier) @name) @definition.class
(abstract_class_declaration name: (type_identifier) @name) @definition.class
(interface_declaration name: (type_identifier) @name) @definition.interface
(type_alias_declaration name: (type_identifier) @name) @definition.type_alias
(enum_declaration name: (identifier) @name) @definition.enum
(enum_body (property_identifier) @name @definition.enum_member)
(enum_body (enum_assignment name: (property_identifier) @name) @definition.enum_member)
(internal_module name: (_) @name) @definition.module
(module name: (_) @name) @definition.module

((method_definition name: (_) @name) @definition.constructor
  (#eq? @name "constructor"))
((method_definition name: (_) @name) @definition.method
  (#not-eq? @name "constructor"))
(abstract_method_signature name: (_) @name) @definition.method
(public_field_definition name: (_) @name) @definition.field

; Members of interfaces and of object type aliases, not of inline types.
(interface_body (method_signature name: (_) @name) @definition.method)
(interface_body (property_signature name: (_) @name) @definition.field)
(type_alias_declaration value: (object_type (method_signature name: (_) @name) @definition.method))
(type_alias_declaration value: (object_type (property_signature name: (_) @name) @definition.field))

; `const f = () => …` and `const f = function () {…}` anywhere.
(variable_declarator
  name: (identifier) @name
  value: [(arrow_function) (function_expression) (generator_function)]) @definition.function

; Other top-level variables (functions win the tie above).
(program (lexical_declaration (variable_declarator name: (identifier) @name) @definition.variable))
(program (variable_declaration (variable_declarator name: (identifier) @name) @definition.variable))
(program (export_statement (lexical_declaration (variable_declarator name: (identifier) @name) @definition.variable)))
(program (export_statement (variable_declaration (variable_declarator name: (identifier) @name) @definition.variable)))
