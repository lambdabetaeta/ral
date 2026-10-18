---
generated_at_commit: 49399cfc
---
# Paths are shown with a forward slash

**Every path string ral produces, a value a builtin returns or text a person or the model reads, is `/`-separated on every host: `C:/blah/bleh`, `//server/share/x`, `src/x/f`.** `core::path::show::shown` is the one place that shows a path; `fold_for` is its separator fold.

- **One value on every host.** A script's `glob` or `cwd` yields the same string on Windows as on Unix, so a test or a model prompt need not know the host.
- **Escapes cannot eat it.** In a `"…"` literal `\n` is a newline; a `C:\new` printed back and pasted would be corrupted. `/` is inert there.
- **Input already reads either.** The resolver accepts both separators, so showing with `/` loses nothing.
- **The verbatim exception.** A `\\?\C:\…` head is dropped (`dunce::simplified`) where the path means the same without it. A path only the OS can read literally (`\\?\UNC\…`, `\\.\pipe\…`, a long or reserved-name verbatim path) is shown exactly as given, since `/` is not a separator there.
- **The native boundary.** A path ral hands to the OS (`PWD`, sandbox rules, DACL calls, internal identity and matching) stays native. Only the text ral shows is folded.

On Unix `shown` is the identity.
