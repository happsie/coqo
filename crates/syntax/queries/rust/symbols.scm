; Rust definitions (tags conventions: @definition.<kind> and @name).

(mod_item name: (identifier) @name) @definition.module
(struct_item name: (type_identifier) @name) @definition.struct
(union_item name: (type_identifier) @name) @definition.struct
(enum_item name: (type_identifier) @name) @definition.enum
(enum_variant name: (identifier) @name) @definition.enum_member
(trait_item name: (type_identifier) @name) @definition.trait
(type_item name: (type_identifier) @name) @definition.type_alias
(impl_item type: (_) @name) @definition.impl
(function_item name: (identifier) @name) @definition.function
(function_signature_item name: (identifier) @name) @definition.function
(const_item name: (identifier) @name) @definition.constant
(static_item name: (identifier) @name) @definition.constant
(macro_definition name: (identifier) @name) @definition.macro
(field_declaration name: (field_identifier) @name) @definition.field
