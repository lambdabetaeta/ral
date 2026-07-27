Everything named below is already installed. `pip3 install --user` can add more from the package list, but nothing installed survives past this conversation — solve the problem with these first.

## Spreadsheets

`in2csv` is the quickest way to see inside a workbook, and `csvlook` prints a table you can read:

    in2csv --names accounts.xlsx                 # the sheet names
    in2csv --sheet #'Q1'# accounts.xlsx > q1.csv
    csvlook --max-rows 12 q1.csv
    csvstat q1.csv                               # per column: type, blanks, min/max

`csvcut`, `csvsort`, `csvgrep`, and `csvjoin` handle most reshaping without Python:

    csvcut -c #'Surname,Email'# staff.csv | csvsort -c Surname > list.csv
    csvjoin -c #'ID'# staff.csv payroll.csv > merged.csv

For anything that must go back into `.xlsx`, use Python — `openpyxl` for cells and formatting, `pandas` when the work is really table algebra:

    python3 << #'
    import openpyxl
    wb = openpyxl.load_workbook("accounts.xlsx")   # formulas come back as text
    ws = wb["Q1"]
    print(ws.title, ws.max_row, ws.max_column)
    print([c.value for c in ws[1]])                # whatever row 1 actually holds
    '#

Three traps that cost real documents:

- **`data_only=True` returns the value Excel last cached** — and `None` if the file was never opened in Excel. Never save a workbook loaded that way: every formula in it becomes a frozen number.
- **openpyxl loses charts and images when it re-saves.** Look before you round-trip: `unzip -l book.xlsx` shows `xl/charts/` and `xl/media/` if any exist. If they do, do not save over that workbook — put your result in a new file and tell the user why.
- **Column widths, merged cells, and number formats survive; conditional formatting and pivot tables largely do not.** Adding a sheet or appending rows is safe. Rewriting a formatted sheet is not.

Set the format when you write, or a date arrives as 45678:

    ws.cell(row=r, column=3, value=total).number_format = "#,##0.00"
    ws.cell(row=r, column=1, value=when).number_format  = "dd/mm/yyyy"

## Word documents

To **read** one, pandoc:

    pandoc -t plain minutes.docx                  # text, for you
    pandoc -t markdown minutes.docx -o minutes.md

To **edit** one and keep how it looks, python-docx. Never convert a `.docx` out to markdown and back — the round trip flattens styles, numbering, and headers:

    python3 << #'
    from docx import Document
    d = Document("letter.docx")
    for p in d.paragraphs:
        if "<<NAME>>" in p.text:
            p.runs[0].text = p.text.replace("<<NAME>>", name)
            for r in p.runs[1:]:
                r.text = ""
    d.save("letter-Smith.docx")
    '#

That shape is deliberate: Word splits a paragraph across runs wherever formatting or a spell-check mark changes, so a placeholder is rarely inside one run. Match on `p.text`, write the whole replacement into the first run, and empty the others; the paragraph keeps the first run's formatting.

To **make** one that looks like the office's own letters, write markdown and let pandoc dress it in an existing document's styles:

    pandoc draft.md --reference-doc=house-style.docx -o letter.docx

Tables and headers also live outside `d.paragraphs` — `d.tables[i].rows[r].cells[c]` and `d.sections[i].header` — so a placeholder you cannot find is probably in one of those.

## PDFs

    pdfinfo report.pdf                            # pages, size, producer
    pdftotext -layout report.pdf -                # text, columns preserved
    pdftotext -layout -f 4 -l 4 report.pdf page4.txt
    pdfimages -list scan.pdf

`-layout` is what makes a table recoverable: it holds the columns apart with runs of spaces, so rows can be split on two-or-more spaces.

    let page  = lines !{pdftotext -layout -f 2 -l 2 fees.pdf - | from-string}
    let data  = filter { |r| re-match #'^\s*\d'# $r } $page
    map { |r| re-replace-all #' {2,}'# #','# !{re-replace-all #'^\s+'# #''# $r} } $data

Always check an extracted table against a total printed in the PDF itself. A `-layout` extraction is a good guess, not a parse: merged cells, wrapped text, and a right-aligned column all break it, and they break it quietly.

`pdftotext` returning nothing means the pages are pictures. OCR first, then read:

    ocrmypdf --language eng --rotate-pages scan.pdf scan-ocr.pdf
    pdftotext -layout scan-ocr.pdf -

OCR is slow — a hundred pages is minutes. `defer` it and raise `timeout_secs`. OCR output is never certain: a name or a figure recovered from a scan should be shown to the user, not quietly filed.

Split, merge, rotate, and unlock with qpdf:

    qpdf --empty --pages cover.pdf 1 report.pdf 1-z -- combined.pdf
    qpdf --split-pages=1 report.pdf page.pdf      # page-01.pdf, page-02.pdf, …
    qpdf --rotate=+90:3 report.pdf turned.pdf

## Converting anything

LibreOffice converts every office format, headless:

    let profile = "-env:UserInstallation=file://$scratch/lo-profile"
    soffice --headless $profile --convert-to pdf --outdir letters-pdf letters/Smith.docx

Three rules. Give it a private `-env:UserInstallation` directory away from the folder, and never run two conversions against the same profile at once — they deadlock. Check that the output file exists afterwards: it exits 0 having written nothing when it dislikes a document. And it writes only the *first* sheet when converting a workbook to csv; use `in2csv --sheet` for the rest.

## Mail merge

A merge is a table, a template, and a loop, done as three visible steps.

1. Read the table and check it: every row has a name, no blanks, no duplicates, the count matches what the user expects.
2. Fill **one** letter, read it back, and show it before doing the rest.
3. Fill the remainder into their own subfolder, and convert to PDF in one deferred pass if the user wants PDFs.

    let rows = from-csv < recipients.csv
    [count: !{length $rows}, first: $rows[0], columns: !{keys $rows[0]}]

Name the outputs after the recipient — `letters/Smith-J-reminder.docx` is findable, `letter-07.docx` is not.

## Renaming and filing in bulk

Never rename inside the same loop that computes the names. Build the plan as data, check it, show it, then carry it out:

    let plan = map { |f| [from: $f, to: "invoices/2026-$f"] } !{glob #'*.pdf'#}

Before moving anything, check the plan three ways: no two destinations are equal, no destination already exists, every source still does. Write the plan into the folder as `rename-plan.csv` before applying it — that file is how the user reverses the change by hand. Then `mv` one pair at a time, so a failure halfway leaves a plan you can read against the folder.

## Pictures and scans

`magick identify photo.jpg` before anything else; a phone photograph is often 8000 pixels wide and will bloat whatever document it is dropped into.

    magick photo.jpg -auto-orient -resize 1600x -quality 85 photo-small.jpg
    magick page-*.jpg receipts.pdf                # a set of photographs into one PDF

## Searching text

`rg` searches text files, but a `.docx` or `.xlsx` is a zip archive, so it finds nothing inside one. Convert to text first, then search that:

    let docs    = map { |f| "== $f\n!{pandoc -t plain $f}" } !{glob #'**/*.docx'#}
    let extract = "$scratch/all-text.txt"
    to-lines $docs > $extract
    rg -n #'(?i)bursary'# $extract

That `$scratch` is the scratch directory Host names: bind it once at the start of the session and keep every extract, conversion, and intermediate file there. A stray `all-text.txt` among someone's documents is litter.
