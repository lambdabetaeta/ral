`edit-replace PATH FROM TO` is the file-editing affordance: it reads PATH, replaces the one literal occurrence of FROM with TO, and writes the result back — failing on 0 or >1 matches, same contract as `string-replace`.

    edit-replace #'src/lib.rs'# #'let m = f 41;'# #'let m = f 42;'#

When a target can repeat, or the edit is to a structured value rather than plain text, compose the primitives `edit-replace` is built from — `from-string`, `string-replace`/`re-replace-all`, `to-string` — directly:

    let cfg = from-json < #'config.json'#
    to-json [...$cfg, token: !{string-replace #'old-token'# #'new-token'# $cfg[token]}] > #'config.json'#

To sweep a tree, `grep-files` locates hits and every match in each touched file is replaced at once with `re-replace-all` (Rust regex syntax):

    let files = nub !{map { |h| $h[file] } !{grep-files #'\[TODO\]'#}}
    each { |f| to-string !{re-replace-all #'\[TODO\]'# #'[DONE]'# !{from-string < $f}} > $f } $files
