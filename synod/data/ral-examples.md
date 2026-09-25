## Examples

The reference above is the whole language; here it is worked through with
the tools of this office image.

    let sheet  = csvlook data.csv
    let rows   = !{csvcut -c name,total data.csv | from-lines}
    attempt { soffice --headless --convert-to pdf letter.docx }; attempt { pandoc notes.md -o notes.pdf }
    let export = { pandoc report.md -o report.pdf }
    if !{succeeds { pandoc --version > /dev/null }} { !$export } else { echo #'pandoc is not installed'# }

`sheet` captures a command's stdout with `let`; `rows` forces an anonymous
block over a pipeline and gives you a list of lines; the two
`attempt`s run in sequence and neither aborts the script if it fails;
`export` names a block without running it; and the `if` reads a probe's
true/false through `succeeds`, forcing `export` — `!$export` — only on the
succeeding branch.

Three more things worth knowing that the reference above, written for
source trees, has no occasion to mention:

- `from-csv` decodes a table into a list of records keyed by the header row,
  every field a `String`; `to-csv` writes one back, with the columns in
  **alphabetical** order, not the order they came in. When column order
  matters to the user, write the file with `csvkit` or Python instead.
- `audit { … }` reads a program that reports through its exit code and
  returns its output as data instead of failing the script. `soffice`,
  `ocrmypdf`, and `qpdf` all need it. Its report is `[outcome, trail]`; read
  it with `succeeded $r` and `!{commands $r}[0][stdout]` rather than by
  walking the trail yourself.
- `glob` and the search builtins skip hidden files. They do not skip Word's
  lock files: a `~$letter.docx` beside `letter.docx` means the user has that
  document open right now. Never open, copy, or convert one, and think
  twice before writing to the document it guards.
