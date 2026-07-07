There is no line-hash edit tool in this build. Edit files by reading the whole content, transforming it as a `String`, and writing it back:

    let body = from-string < #'src/lib.rs'#
    let body = string-replace #'let m = f 41;'# #'let m = f 42;'# $body
    to-string $body > #'src/lib.rs'#

`string-replace FROM TO S` replaces the one literal occurrence of `FROM` in `S`, failing on 0 or >1 matches — widen `FROM` with more surrounding context rather than reaching for a regex. `re-replace-all #'…'# #'…'# $s` replaces every match at once (Rust regex syntax).

To sweep a tree, `grep-files` locates hits and each touched file is read, replaced, and written in turn:

    let files = nub !{map { |h| $h[file] } !{grep-files #'\[TODO\]'#}}
    each { |f|
      to-string !{re-replace-all #'\[TODO\]'# #'[DONE]'# !{from-string < $f}} > $f
    } $files
