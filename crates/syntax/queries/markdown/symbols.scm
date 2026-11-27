; Markdown headings. An ATX heading's section holds the sections of its
; sub-headings, so the outline nests by heading level.

(section (atx_heading (inline) @name)) @definition.heading
(setext_heading (paragraph (inline) @name)) @definition.heading
