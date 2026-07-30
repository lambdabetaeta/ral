---
status: active
---

# A service-hosted guest's console is teed to disk and quoted in the failure

**A diagnostic written to a handle that goes nowhere is not a diagnostic, so the
guest's console pump tees — `stdout`, a bounded per-machine log in the backend's
own cache, and a short ring of the last lines in memory — and the boot timeout
*quotes* that ring and names that log rather than telling the reader to look
"above", where under an installed synod there is no above.** Realised in
`vm-manager/src/hcs/console.rs` and `vm-manager/src/hcs/mod.rs`; the broker
protocol is deliberately untouched.

## Context

On an installed synod the process that owns the machine is `SynodMachineBroker`,
a `LocalSystem` service ([[decisions/260725_windows-machine-broker|windows-machine-broker]]),
and a service has no console: its standard output is a handle that writes
nowhere. The pump wrote only there, and stopped pumping on its first failed
write — which under a service is the first chunk. So the one line explaining why
a guest refused to come up was written and discarded, every time, while the boot
failure went on to say the reason was "above".

The refusal recorded in
[[decisions/260730_boot-contract-is-versioned|boot-contract-is-versioned]] is
exactly the case that needs that line: the guest said, correctly and in one
sentence, that it did not know a `ral.` key it had been handed, and then powered
off. Nothing of it reached the person watching the host.

## Decision

- **Three sinks, one pump.** `stdout`, for a maintainer running
  `synod-machine-broker.exe --console`; a log file, for everyone else; and a
  `Tail` ring, so a failure can quote the guest instead of pointing at a place
  the reader has to go and open. A failing `stdout` drops that sink and keeps the
  other two, since stopping there is exactly how the durable copy was lost.
- **The log lives in the cache the backend already owns**, as
  `synod-console-<machine id>.log` — `%ProgramData%\Synod\Machine` under the
  service, which is the one directory `LocalSystem` and an unprivileged window
  agree on and the one this backend is entitled to write in at all.
- **Each sink is bounded by construction**, so a diagnostic does not become
  litter the way session disks did
  ([[decisions/260730_session-disk-outlives-its-machine|session-disk-outlives-its-machine]]):
  `RETAINED_LINES` lines in the ring, none longer than `LINE_LIMIT` characters;
  `LOG_LIMIT` bytes in the file, saying in the file where it stopped; and stale
  logs swept at `LOG_LIFETIME` whenever a console is opened.
- **The log is kept by the head, not the tail.** A boot explains itself at its
  beginning — the kernel's first words, the daemon's refusal — and a guest that
  loops afterwards would otherwise push that beginning out of the file.
- **A failed boot keeps its log; a successful one discards it.** One bool on the
  guest (`dialled`) tells the two otherwise identical teardowns apart: a boot
  that failed *named* its log in the sentence it failed with, so that file has a
  reader still to come, while a boot that worked has explained itself by working.
- **The failure says one of three things, and each leaves the reader somewhere
  different** (`console_says`): the guest's last words, quoted oldest-first, which
  is usually the whole diagnosis; that the guest said nothing at all, which is
  itself the finding, with the log named for whoever wants to confirm the
  emptiness; or that no console could be opened, which is a fault in synod and is
  said as one rather than implying the guest was silent.
- **The broker protocol does not change.** `broker::VERSION` stays 2: the boot
  error string already crosses the pipe to the window as the `Reply`'s own
  failure, so the quoted lines and the log's path arrive with no new frame, no new
  request, and no new field. A version bump is a compatibility event between an
  installed service and an installed window, and paying one to carry text that
  already travels would be paying it for nothing.

## Consequences

- **A boot failure on a shipped machine is diagnosable from the window**, in the
  sentence the window already shows, and completely from the log path in that
  sentence.
- **The console stops being a maintainer's privilege.** `--console` remains the
  live view, but it is no longer the only one, so a boot fault no longer has to
  be reproduced in a checkout before it can be read.
- **`Console` grew a cache argument**, which is how `boot` passes down the one
  directory the backend may write; the pipe, the machine, and the log now all take
  their name from the same machine id.

## Open questions

- **The ring is a handful of lines.** A cause that scrolls past — a module that
  failed to load thirty lines before the mount that failed because of it — is in
  the log but not in the sentence, and how many lines a failure should quote is a
  judgement rather than a measurement.
- **Nothing surfaces the console into the review screen.** The error sentence is
  the only path; whether a failed boot deserves a card carrying the guest's words
  is unasked.

## See also

[[decisions/260730_boot-contract-is-versioned|boot-contract-is-versioned]] (the
failure this made audible, and the build check that stops it recurring),
[[decisions/260725_windows-machine-broker|windows-machine-broker]] (why the owner
of the machine is a service with no console, and what crossing its pipe costs),
[[decisions/260725_windows-hyper-v-backend|windows-hyper-v-backend]] (the machine
and its COM-port console),
[[decisions/260730_session-disk-outlives-its-machine|session-disk-outlives-its-machine]]
(the same cache, and the rule the log's bounds were written under),
[[design/failure|failure]] (a refusal is data, and must reach a reader),
[[map/synod|synod]] (where the console and the backend live).

Cite: `vm-manager/src/hcs/console.rs` (`Console::create`, `Tail`, `open_log`,
`sweep`, `keep_logging`), `vm-manager/src/hcs/mod.rs` (`console_says`, `quoted`,
`Guest::dialled`), `vm-manager/src/broker/mod.rs` (`VERSION`, unchanged).
