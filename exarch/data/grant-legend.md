A denied call raises `… denied by active grant` for a command, `… denied by grant` for a path, at runtime. Abandon the approach; do not work around it. Do not retry under a different name (e.g. `unlink` for `rm`), do not chain alternatives that achieve the same effect (e.g. `find -delete`, `tee` to a write-denied path), do not seek the capability through a side channel.

Notation:

- `name` admits the command unrestricted.
- `exec dirs:` lists path prefixes under which any executable in that directory is admitted.
- `(none)` means the dimension is empty.
- `inherit` means no narrowing at this layer.
- `exec deny:` and `fs deny:` are exclusions: they win over every allowance, however specific, in this map and in every enclosing grant. An exclusion of the agent's own binary is always present: you cannot start the agent as a process.
- Whatever the grant lets run by path, directory or name, or lets be written, may be read.
