; Rust folds.

[
  (block)
  (declaration_list)
  (field_declaration_list)
  (ordered_field_declaration_list)
  (enum_variant_list)
  (match_block)
  (use_list)
  (token_tree)
  (arguments)
  (parameters)
  (array_expression)
  (field_initializer_list)
  (macro_definition)
  (string_literal)
  (raw_string_literal)
] @fold

[
  (line_comment)
  (block_comment)
] @fold.comment

(use_declaration) @fold.imports
