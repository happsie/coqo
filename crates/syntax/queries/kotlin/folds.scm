; Kotlin folds.

[
  (class_body)
  (enum_class_body)
  (function_body)
  (control_structure_body)
  (lambda_literal)
  (when_expression)
  (value_arguments)
  (function_value_parameters)
  (primary_constructor)
  (string_literal)
] @fold

[
  (line_comment)
  (multiline_comment)
] @fold.comment

(import_header) @fold.imports
