; JavaScript and JSX definitions (tags conventions: @definition.<kind> and
; @name).

(function_declaration name: (identifier) @name) @definition.function
(generator_function_declaration name: (identifier) @name) @definition.function
(class_declaration name: (identifier) @name) @definition.class

((method_definition name: (_) @name) @definition.constructor
  (#eq? @name "constructor"))
((method_definition name: (_) @name) @definition.method
  (#not-eq? @name "constructor"))
(field_definition property: (_) @name) @definition.field

; `const f = () => …` and `const f = function () {…}` anywhere.
(variable_declarator
  name: (identifier) @name
  value: [(arrow_function) (function_expression) (generator_function)]) @definition.function

; Other top-level variables (functions win the tie above).
(program (lexical_declaration (variable_declarator name: (identifier) @name) @definition.variable))
(program (variable_declaration (variable_declarator name: (identifier) @name) @definition.variable))
(program (export_statement (lexical_declaration (variable_declarator name: (identifier) @name) @definition.variable)))
(program (export_statement (variable_declaration (variable_declarator name: (identifier) @name) @definition.variable)))
