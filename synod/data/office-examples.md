## Office examples

Inspect a table before choosing a transformation:

    let rows = from-csv < recipients.csv
    [rows: !{length $rows}, sample: !{take 3 $rows}]

The binding keeps the whole table; the turn returns only its size and a
sample. CSV fields are strings, including numbers. `to-csv` writes columns
in alphabetical order; use Python or csvkit when their order must be preserved.

After inspecting `draft.md`, convert it to a new Word document and reopen
the result:

    let output = #'letter.docx'#
    let conversion = audit {
      if !{exists $output} { fail [status: 1, message: #'`letter.docx` already exists; choose another output name.'#] }
      pandoc draft.md -o $output
      let text = pandoc -t plain $output | from-lines
      [file: $output, lines: !{length $text}, preview: !{take 10 $text}]
    }
    $conversion[outcome]

Inspect the outcome before reporting success. A successful outcome includes
text read back from the new document; compare it with the draft and the
requested changes. On failure, `commands $conversion` retains each executed
command's status, stdout, and stderr. Resume from the failed step after
inspecting any output already written.

A file named `~$letter.docx` is Word lock metadata, not a document to process.
Leave it out of searches and conversion lists; check whether the associated
document is in use before modifying it.
