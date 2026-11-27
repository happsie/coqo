; Kotlin definitions (tags conventions: @definition.<kind> and @name).
; The grammar has few field names, so patterns match by position. When two
; patterns match the same definition, the earlier one wins.

(class_declaration "interface" (type_identifier) @name) @definition.interface
(class_declaration (type_identifier) @name (enum_class_body)) @definition.enum
(class_declaration (type_identifier) @name) @definition.class
(object_declaration (type_identifier) @name) @definition.class
(companion_object (type_identifier) @name) @definition.class
(enum_entry (simple_identifier) @name) @definition.enum_member
(type_alias (type_identifier) @name) @definition.type_alias
(secondary_constructor "constructor" @name) @definition.constructor
(function_declaration (simple_identifier) @name) @definition.function

; `const val` anywhere, class properties, `val`/`var` constructor
; parameters, and top-level properties.
((property_declaration
  (modifiers (property_modifier) @_const)
  (variable_declaration (simple_identifier) @name)) @definition.constant
  (#eq? @_const "const"))
(class_body (property_declaration (variable_declaration (simple_identifier) @name)) @definition.field)
(class_parameter (binding_pattern_kind) (simple_identifier) @name) @definition.field
(source_file (property_declaration (variable_declaration (simple_identifier) @name)) @definition.variable)
