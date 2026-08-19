`view-text PATH START END` shows the half-open line range `[START, END)`. The result is a list of `[line, hash, text]` records, where `<hash>` is a unique freshness witness for that line, which depends on neighbouring lines. Bind and collect in some record to read multiple locations at once:

    let tui-start  = view-text #'src/tui.rs'# 100 150
    let tui-end    = view-text #'src/tui.rs'# 300 350
    [ #'src/tui.rs-100-150'# : $tui-start, #'src/tui.rs-300-350'# : $tui-end ]

`view-text-around PATH LINE PEEK` shows the `2*PEEK + 1` lines centred on `LINE`, in the same records.

`edit-hash PATH EDITS` applies a batch of `EDITS`, a list of records `[hash: HASH, line: NEWTEXT]`. Each edit replaces ONLY the line identified by `HASH` verbatim with `NEWTEXT`. It is atomic: every hash is resolved against lines before editing; and a batch either applies whole or fails whole. Use raw strings `#'…'#` for `NEWTEXT` without any escapes.

There are three ways to use `edit-hash`. To delete a line pass the empty string `#''#` as `NEWTEXT`. To replace a line pass a new line as `NEWTEXT`; the newline will be preserved. To replace a line with multiple
new lines put several newline characters (not escapes) in `NEWTEXT`. The
replacement must already have the exact indentation needed at the insertion point; write it directly with a raw string at the target indentation, or use `!{indent N !{dedent #'...'#}}` to author at natural indentation then shift. Example:

    edit-hash #'src/lib.rs'# [
      [hash: h1b2c3, line: #'        let m = f {
            let scaled = n * 2;
            g 42
        }'#],
      [hash: h4e5f6, line: #'    let m = 0'#],         # replace a line, keeping the newline at the end
      [hash: h7a8b9, line: #''#],                      # delete a line
    ]
    edit-hash #'src/pointer.rs'# [ [hash : h3af4d, #''# ]   # you can edit multiple files in the same ral script

Edits with newlines DO NOT replace the lines that follow; you MUST mention the hash of every line you wish to change.
