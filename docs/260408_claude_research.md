# Fifty years of broken shells — and what comes next

**The UNIX shell is the most consequential piece of software almost nobody considers broken.** Born in 1971 with Ken Thompson's original shell and refined through the Bourne shell (1979), csh (1978), ksh (1983), and bash (1989), the shell's core abstractions — text-based pipes, implicit word splitting, fork-per-command execution, and stringly-typed everything — have remained essentially unchanged for half a century. These design decisions, reasonable for interactive use on PDP-11s, now constitute the single largest source of preventable bugs, security vulnerabilities, and operational failures in computing. With AI agents increasingly executing shell commands autonomously, these problems are no longer merely annoying — they are dangerous. This report catalogs every major shell pitfall, evaluates what modern shells are doing to fix them, and defines the desiderata for a shell fit for both humans and machines.

---

## The original sins: word splitting, quoting, and silent failure

The shell's most devastating design decision is **implicit word splitting and globbing on every unquoted variable expansion**. When you write `rm $file` where `file="my document.txt"`, the shell splits this into two arguments: `rm my document.txt` — attempting to delete two nonexistent files. When `var='10 * 4'` is used unquoted in a loop, the `*` expands to every filename in the current directory. ShellCheck's SC2086 ("double quote to prevent globbing and word splitting") is the single most common shell script error. This is not a bug — it is the intended design of every Bourne-family shell, and suppressing it requires explicit quoting on every variable reference.

Quoting itself is a labyrinth. Single quotes preserve everything literally but cannot contain a single quote — forcing the grotesque `'it'\''s'` construction. Double quotes allow variable expansion but only escape five specific characters. Nested command substitution with backticks requires exponential escaping. The csh family is worse: you cannot escape `$` inside double quotes at all (`set foo = "this is a \$dollar"` produces `dollar: Undefined variable`). PowerShell chose the backtick as its escape character, baffling everyone from every other language tradition. The fundamental problem is that shells have **multiple overlapping quoting mechanisms with inconsistent, context-dependent rules** and no clean composition.

Error handling is perhaps the most insidious failure. By default, **shells silently swallow errors and continue execution**. If `mkdir /backup/db` fails, `mysqldump mydb > /backup/db/dump.sql` runs anyway, writing to a nonexistent directory. The supposed fix, `set -e` (errexit), is riddled with exceptions so severe that bash experts agree you should never rely on it. Commands inside `if` conditions are exempt — `set -e; myfunc() { false; echo "still running"; }; if myfunc; then echo True; fi` prints "still running" and "True." Commands before `||` are exempt. Different shells (bash, dash, ksh, zsh) implement `set -e` differently. In pipelines, `false | true` succeeds because only the last command's exit code is checked; `pipefail` helps but is a bash extension, not POSIX. Exit codes are limited to 0–255 with no structured error information, no stack traces, and no exception types.

---

## Variable scoping, portability, and the programming language that isn't

Shell variables are **global by default** with dynamic scoping — a design choice abandoned by virtually every language since early Lisp. If `func1` declares `local var` and calls `func2`, `func2` sees `func1`'s local `var`. More dangerously, each command in a pipeline runs in a subshell, so modifications are silently lost:

```bash
count=0
echo -e "1\n2\n3" | while read num; do count=$((count + num)); done
echo "count=$count"  # Prints 0, not 6
```

This subshell trap is one of the most frequently asked questions on Stack Overflow. Command substitution (`$(...)`) also creates subshells, meaning any function that returns data via echo-and-capture loses all side effects. PowerShell uses dynamic scoping with copy-on-write, where typos silently create new variables (`$variable = 10; $varable = 20` — no error).

**Portability across shells is nearly impossible.** Bash arrays, `[[ ]]` tests, `$'...'` ANSI quotes, process substitution, brace expansion, and even `local` are not POSIX-standard. macOS permanently ships Bash 3.2 (GPLv3 licensing) and defaults to zsh since Catalina. Alpine Linux (powering millions of Docker containers) uses busybox ash. Debian/Ubuntu use dash as `/bin/sh`. Scripts with bashisms fail silently or with cryptic errors. As Larry Wall observed: **"It's easier to port a shell than a shell script."**

As a programming language, shells lack virtually every feature expected of modern software development: no real data types (everything is a string), no floating-point math, no modules or namespaces, no proper function return values (only exit codes 0–255), no exception handling, no standard library, no threading model, no named function parameters, and only one-dimensional arrays even in bash. The debugging experience is terrible — `set -x` is the primary tool, with no breakpoints, no stepping, no variable watches, and cryptic error messages that point to the wrong line. Configuration files form a confusing matrix: interactive login shells read `~/.bash_profile`, interactive non-login shells read `~/.bashrc`, and the standard workaround (source `.bashrc` from `.bash_profile`) exists precisely because the default behavior is wrong.

Performance is inherently limited by the fork/exec model — every external command spawns a new process. Bash can be **10–1,000× slower than C** for compute-intensive tasks. PowerShell is ~80× slower to start than sh due to .NET runtime initialization. Unicode support is incomplete across the board: `tr` cannot handle multibyte characters, glob patterns behave unpredictably with Unicode depending on locale, and filenames on Linux are just bytes — not guaranteed to be valid UTF-8.

---

## Security: a fifty-year track record of vulnerabilities

### Shellshock and the architecture of insecurity

Shellshock (CVE-2014-6271, CVSS 10.0) demonstrated that the shell's design is fundamentally hostile to security. Bash's feature of importing function definitions from environment variables was implemented with a parsing flaw: when Bash encountered `() {` in an environment variable value, it parsed the function definition but **continued executing any commands appended after the closing brace**. The test vector `env x='() { :;}; echo vulnerable' bash -c "echo test"` achieved arbitrary code execution before the intended command even ran. Apache CGI servers, SSH ForceCommand, and DHCP clients were all attack vectors. The initial fix was incomplete — five additional CVEs followed, all rooted in parser weaknesses.

But Shellshock is merely the most famous symptom of a deeper disease. **Shell injection (CWE-78)** is possible whenever applications construct shell commands using unsanitized input, because shells interpret metacharacters (`;`, `|`, `$()`, `` ` ``, `>`) in every string by design. The entire purpose of a shell is to interpret text as commands — there is no type-level distinction between data and code. `eval` introduces a second parsing layer where previously-safe content becomes executable. Arithmetic evaluation (`$(( ))`) recursively evaluates variable values, enabling code execution through array subscripts — an extremely subtle vulnerability.

Filenames are weapons. A file named `-rf` processed by `rm *` becomes `rm -rf`. Files containing shell metacharacters (`;`, `|`, `$()`) can trigger command execution in tools that pass filenames to shell commands. CVE-2025-64756 demonstrated this in the `glob` npm package (10M+ weekly downloads), where a file named `$(touch injected_poc)` triggered arbitrary code execution. SUID shell scripts are fundamentally broken due to a race condition between the kernel starting the interpreter and the shell opening the script file, plus amplified IFS and PATH manipulation attacks.

### What secure shell design requires

The SHILL project (Harvard, OSDI 2014) remains the most rigorous academic answer: a shell where scripts receive explicit capabilities (file handles, directory handles) with fine-grained privileges rather than ambient authority. Built on FreeBSD's Capsicum framework, SHILL demonstrated that a 22-line capability-safe script could sandbox a grading workflow. Perl's taint mode — which marks all external input as "tainted" and prevents its use in `system()`, `exec()`, or `eval()` without explicit validation — remains the gold standard for data-flow tracking that no mainstream shell has replicated. Modern Linux offers seccomp-BPF, AppArmor/SELinux, and OpenBSD's pledge/unveil, but these operate at the OS level, not the shell language level.

---

## What modern shells are doing differently

A new generation of shells has emerged, each attacking the problem from a different angle. The academic paper "Unix Shell Programming: The Next 50 Years" (Greenberg et al., HotOS 2021) formally identifies the fundamental shell problems: error-proneness, non-scaling performance, no support for parallelism, and redundant recomputation. Here is how each major alternative addresses them.

**Nushell** makes the boldest break from tradition. Every command returns structured data — `ls` produces a table with typed columns (name, type, size, modified), not text. Pipelines pass tables, records, and lists between commands. The type system includes **domain-specific types** like `filesize` (with units: kb, mb, gb) and `duration` (sec, min, hr). Built-in parsing for JSON, YAML, TOML, CSV, XML, SQLite, and Parquet eliminates the need for `jq`, `yq`, or `csvkit`. Error handling uses `try/catch` with type mismatches caught at parse time and precise error messages with source-location spans. Values are immutable by default. The trade-off is complete POSIX incompatibility — you cannot paste bash one-liners.

**Oils (OSH/YSH)** uniquely provides a migration path. OSH runs existing bash scripts (more correctly than bash in some edge cases). YSH, activated via `shopt --set ysh:all`, fixes the fundamental problems: **Simple Word Evaluation** eliminates implicit word splitting and globbing. `strict_errexit` addresses the "disabled errexit quirk" where errors inside `if` conditions are silently swallowed. `command_sub_errexit` catches exit codes lost in command substitutions. Static parsing (the input is never rescanned) eliminates Shellshock-class vulnerabilities. YSH adds `var`/`const`/`setvar` keywords, Python/JavaScript-like expressions, `proc` (for commands with I/O) and `func` (for pure functions), and Eggex — readable, composable regular expressions.

**Fish** focuses on sane defaults and discoverability. It has **no word splitting on variable expansion** — the single biggest safety improvement. Real-time syntax highlighting, autosuggestions from history, rich tab completions for thousands of commands, web-based configuration, lexical (not dynamic) scoping, and universal variables that persist across sessions. Fish deliberately sacrifices POSIX compatibility for a coherent design.  Rewritten in Rust for Fish 4.0, it is probably the most popular non-POSIX shell.

**Elvish** introduces dual pipeline channels: traditional byte pipelines alongside **value pipelines** that pass structured data (strings, lists, maps, closures, numbers). Command failures abort execution by default — exceptions, not silent continuation. Undefined variables are caught at compile time: `rm -rf $projetc/bin` produces "variable $projetc not found" before execution. First-class functions, closures, lexical scope, namespaces, and a module system make it a genuine programming language.

**Murex** takes the most pragmatic approach to interoperability: it adds **type annotations to byte streams** without breaking POSIX pipe compatibility. Existing commands work unmodified, but Murex enriches the data with metadata about format (JSON, YAML, CSV), enabling intelligent autocomplete and type-aware manipulation. It includes a built-in unit testing framework and requires variable declaration.

**PowerShell** pioneered the object pipeline in 2006: `Get-Process` returns .NET objects with properties and methods, not text. Commands bind parameters via type contracts (ISA) and property-name contracts (HASA). The `-WhatIf` and `-Confirm` parameters enable safe execution previews. Native transaction support (`Start-Transaction`, `Undo-Transaction`, `Complete-Transaction`) is a model no other shell has replicated. Weaknesses include **~80× slower startup** than sh, verbose syntax, and loss of object richness when interacting with external commands (which still produce text).

**Plan 9's rc shell**, designed by Tom Duff at Bell Labs, solved the most important problem decades ago: **no word splitting on variable substitution**. Variables store lists of strings, not single strings, and substitution preserves list structure. Only single quotes exist (doubled for literals: `'How''s your father?'`). Input is never rescanned except by `eval`. The IFS attack vector is eliminated entirely. rc's influence runs through Oils, Elvish, and the es shell.

---

## What AI agents need from shells

The terminal has become the primary interface for AI coding agents. Claude Code, Gemini CLI (88K+ GitHub stars), Open Interpreter, Codex CLI, Aider, and Warp Terminal's Agent Mode all execute shell commands autonomously. Stanford's Terminal-Bench project bets that "95% of LLM-computer interaction will be through a terminal-like interface." But current shells are hostile to AI agents in specific, identifiable ways.

**Structured output is the single most important requirement.** AI agents parsing text output face table layouts that change with terminal width, inconsistent field formats ("3 days" vs. seconds), and progress bars polluting parseable data. The fix is simple: `--json` to stdout, everything else to stderr. AWS CLI's `--cli-error-format json` is the gold standard. Eight rules for agent-friendly CLI design have emerged from practitioner experience: flat over nested JSON, consistent types across commands, JSON Lines for streaming, meaningful exit codes beyond 0/1, `--quiet` for bare values, and combined structured errors with error code, message, and contextual fields.

**Sandboxing is non-negotiable.** LLMs are probabilistic systems that cannot serve as a Trusted Computing Base — a reference monitor that correctly denies 99% of unauthorized access is still exploitable. Real incidents demonstrate the risk: Replit's AI deleted a production database despite code-freeze instructions; Gemini CLI deleted user files and hallucinated about their locations; the "Terminal DiLLMa" attack uses ANSI escape sequences for prompt injection. NVIDIA's AI Red Team mandates: block arbitrary network egress (prevents exfiltration), block file writes outside workspace, block writes to config files, and prefer full virtualization. The isolation hierarchy runs from nsjail/Bubblewrap (process-level) through Docker+seccomp (container) to gVisor (user-space kernel) to Firecracker microVMs (hardware virtualization).

**Rollback and undo mechanisms are critical.** IBM's STRATUS system (NeurIPS 2025) implements Transactional-No-Regression: every action has a corresponding undo operator, commands are simulated first, and if system state worsens after execution, automatic rollback occurs — achieving **150%+ improvement** over state-of-the-art on AIOpsLab benchmarks. Replit's Snapshot Engine uses Git checkpoints and copy-on-write storage for instant filesystem forks, enabling parallel sampling where multiple agents try the same problem in isolated forks and the best result is selected (**8% SWE-bench improvement**). PowerShell's native transaction support demonstrates the model other shells should adopt.

**Predictable, consistent behavior enables reliable automation.** A noun→verb command hierarchy (`myctl user create` instead of `create-user`) turns `--help` exploration into deterministic tree search. Idempotent operations are critical — `myctl ensure namespace prod` succeeds whether the namespace exists or not, while `myctl create namespace prod` fails on retry. The kubectl `apply` model (declarative, idempotent) is the proven pattern. Capability-based security must move from OS-level to shell-level: per-tool permission scoping, time-bounded access (capability valid for ~500ms if task takes ~300ms), and purpose-bounded permissions that prevent exfiltration chains.

---

## Rated desiderata for a modern shell

The following table synthesizes all findings into a prioritized list of what a modern shell should provide. Importance ratings reflect how critical each property is for correctness, security, and AI-agent compatibility.

| # | Desideratum | Importance | Rationale |
|---|------------|------------|-----------|
| 1 | **No implicit word splitting or globbing** | **High** | The #1 source of shell bugs. Fish, rc, Oils, Nushell, Elvish all eliminate it. No credible modern shell retains it. |
| 2 | **Structured data in pipelines** | **High** | Eliminates brittle text parsing for both humans and AI. Nushell, PowerShell, Elvish, and Murex converge on this. Essential for AI agent parsing. |
| 3 | **Proper error handling by default** | **High** | Silent failure is unacceptable. Exceptions or explicit error values, not `set -e`. Elvish aborts on failure by default; Oils provides `strict_errexit` + `try`/`catch`. |
| 4 | **Static parsing (no re-scanning)** | **High** | Eliminates Shellshock-class vulnerabilities and makes the language predictable. rc, Oils, Elvish all implement this. |
| 5 | **Capability-based security / sandboxing** | **High** | Essential for AI agent safety. Scripts should receive only the privileges they need. SHILL, Capsicum, and Leash demonstrate feasibility. |
| 6 | **Machine-parseable structured output** | **High** | `--json` to stdout, human messages to stderr, meaningful exit codes (not just 0/1), structured error objects. The #1 AI agent requirement. |
| 7 | **Real type system** | **High** | At minimum: strings, integers, floats, booleans, lists, maps/records. Nushell adds filesize and duration types. Eliminates "everything is a string" bugs. |
| 8 | **Sane quoting and escaping** | **High** | One clean quoting mechanism, not four overlapping ones. rc uses only single quotes. Oils adds raw strings and J8 strings. |
| 9 | **Transactional semantics / undo / rollback** | **High** | Critical for AI agent safety. Every state change should be reversible. PowerShell has native transactions; IBM STRATUS demonstrates the pattern for AI. |
| 10 | **Sandboxed execution environment** | **High** | AI agents must run in isolated containers or VMs with network egress controls and filesystem restrictions. Non-negotiable for production deployment. |
| 11 | **Proper functions with lexical scoping** | **High** | Dynamic scoping is a proven source of bugs. Fish, Elvish, and Nushell all use lexical scoping. Functions should support named parameters and return structured data. |
| 12 | **Idempotent operations** | **High** | Commands must be safe to retry. Declarative (`ensure`, `apply`) over imperative (`create`, `delete`). Critical for AI agents that retry on failure. |
| 13 | **Consistent, portable behavior** | **Medium** | Cross-platform consistency matters, but breaking POSIX compatibility is acceptable (and necessary) for a clean design. Provide a migration path like Oils does. |
| 14 | **Module / namespace system** | **Medium** | Eliminates global namespace pollution from `source`. Elvish has modules and packages; PowerShell has a mature module ecosystem. |
| 15 | **Comprehensive audit trail** | **Medium** | Every command logged with identity, parameters, timing, and decision context. Essential for AI agent compliance (EU AI Act, HIPAA, SOX). |
| 16 | **Good debugging experience** | **Medium** | Stack traces, breakpoints, variable watches, conditional breakpoints. No shell today does this well. A proper debugger protocol would transform shell development. |
| 17 | **Rich interactive UX** | **Medium** | Syntax highlighting, autosuggestions, contextual completions. Fish sets the standard. Important for human productivity but less relevant for AI agents. |
| 18 | **`--dry-run` for destructive operations** | **Medium** | Preview-before-execute pattern. PowerShell's `-WhatIf` is the model. Critical for AI agent safety when combined with human-in-the-loop confirmation. |
| 19 | **Taint tracking for untrusted input** | **Medium** | Perl's taint mode marks external input and prevents its use in dangerous operations without validation. No mainstream shell has this — a critical security gap. |
| 20 | **Natural language integration** | **Medium** | Built-in NL-to-command translation with confirmation. Warp, Microsoft AI Shell, and Neural Shell demonstrate the pattern. Useful but not foundational. |
| 21 | **Parallel execution primitives** | **Medium** | Beyond `&` and `wait`. Nushell's `par-each`, structured concurrency, and process pools. Research projects PaSh and POSH demonstrate automatic parallelization. |
| 22 | **Sane configuration model** | **Medium** | One configuration file, not a matrix of `.bashrc`/`.bash_profile`/`.profile`/`.bash_login`. Fish uses a single `config.fish`. |
| 23 | **First-class Unicode support** | **Low** | Important but largely an implementation detail. Modern languages handle this natively; shells should too. |
| 24 | **Performance / no fork-per-command** | **Low** | Matters for compute-intensive tasks, but shells should delegate those to proper languages. Murex uses threads for builtins. Nushell is written in Rust. |
| 25 | **Decision traces for AI explainability** | **Low** | Full provenance recording of context, reasoning, and alternatives. Valuable for enterprise compliance but not a shell-level primitive — better handled by the agent framework. |

---

## Conclusion: the shell must become a platform, not just an interpreter

The fifty-year history of shell problems reveals a consistent pattern: **design decisions optimized for interactive human use on 1970s hardware become liabilities when shells are used as programming languages, security boundaries, or AI agent interfaces.** Word splitting, implicit globbing, text-only pipes, silent error swallowing, dynamic scoping, and the absence of any distinction between code and data are not bugs — they are fundamental architectural choices that cannot be fixed without breaking backward compatibility.

The modern shell landscape has converged on clear answers. No credible new shell retains implicit word splitting. Structured data pipelines are the consensus replacement for text-only pipes, with Nushell (tables), PowerShell (objects), Elvish (values), and Murex (typed bytes) offering different trade-offs on the interoperability spectrum. Static parsing eliminates Shellshock-class vulnerabilities. Lexical scoping replaces dynamic scoping. Exceptions replace silent failure.

The AI agent revolution adds three non-negotiable requirements that no current shell fully satisfies: **structured output as a first-class contract** (not an afterthought flag), **capability-based sandboxing at the language level** (not just OS-level containers), and **transactional semantics with rollback** (not just logging). IBM's STRATUS and Replit's Snapshot Engine prove these are achievable with dramatic effectiveness gains.

The shell that will win the next fifty years will not be the one with the cleverest syntax. It will be the one that treats **safety, structure, and reversibility** as foundational properties rather than optional features — a platform where both humans and AI agents can operate with confidence that commands do what they mean, errors are caught before they propagate, and mistakes can be undone.