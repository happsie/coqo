; Java definitions (tags conventions: @definition.<kind> and @name).

(class_declaration name: (identifier) @name) @definition.class
(record_declaration name: (identifier) @name) @definition.class
(interface_declaration name: (identifier) @name) @definition.interface
(annotation_type_declaration name: (identifier) @name) @definition.interface
(enum_declaration name: (identifier) @name) @definition.enum
(enum_constant name: (identifier) @name) @definition.enum_member
(constructor_declaration name: (identifier) @name) @definition.constructor
(compact_constructor_declaration name: (identifier) @name) @definition.constructor
(method_declaration name: (identifier) @name) @definition.method
(annotation_type_element_declaration name: (identifier) @name) @definition.method
(field_declaration declarator: (variable_declarator name: (identifier) @name)) @definition.field
(constant_declaration declarator: (variable_declarator name: (identifier) @name)) @definition.constant
