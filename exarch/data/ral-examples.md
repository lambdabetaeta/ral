## Examples

The reference above is the whole language; here it is worked through with
the tools of a source tree.

    let branch = git branch --show-current
    let log    = !{git log --oneline -20 | from-lines}
    attempt { cargo fmt --check }; attempt { cargo clippy -q }
    let build  = { cargo build --release }
    if !{succeeds { cargo check -q }} { echo #'clean'# } else { !$build }

`branch` captures a command's stdout with `let`; `log` forces an anonymous
block over a pipeline and gives you a lazy stream of lines; the two
`attempt`s run in sequence and neither aborts the script if it fails; `build`
names a block without running it; and the `if` runs `cargo check` only to
read its true/false through `succeeds`, forcing `build` — `!$build` — only
on the failing branch.
