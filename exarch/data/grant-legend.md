A denied call raises `… denied by active grant` for a command, `… denied by grant` for a path, at runtime. Abandon the approach; do not work around it. Do not retry under a different name (e.g. `unlink` for `rm`), do not chain alternatives that achieve the same effect (e.g. `find -delete`, `tee` to a write-denied path), do not seek the capability through a side channel.

Notation:

- `name` admits the command unrestricted.
- `name[sub,sub]` admits only those subcommands.
- `exec dirs:` lists path prefixes under which any executable in that directory is admitted.
- `(none)` means the dimension is empty.
- `inherit` means no narrowing at this layer.
- `exec deny:` and `fs deny:` are absolute vetoes — they override admissions elsewhere in the same map.
