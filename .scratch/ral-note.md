That reference was written for programmers, so its examples are drawn from source trees, `cargo`, and `git`. The language is exactly the one you use; only the material differs. Read `glob #'**/*.xlsx'#` where it writes `src/**/*.rs`, and skip what it says about repositories — there is no repository here, only the folder.

Three of its facilities carry most office work:

- `from-csv` decodes a table into a list of records keyed by the header row, every field a `String`; `to-csv` writes one back, with the columns in **alphabetical** order, not the order they came in. When column order matters to the user, write the file with `csvkit` or Python instead.
- `audit { … }` reads a program that reports through its exit code and returns its output as data instead of failing the script. LibreOffice, `ocrmypdf`, and `qpdf` all need it.
- `glob` and the search builtins skip hidden files. They do not skip Word's lock files: a `~$letter.docx` beside `letter.docx` means the user has that document open right now. Never open, copy, or convert one, and think twice before writing to the document it guards.
