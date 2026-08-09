---
status: proposed
---

# Pipes are positional byte wires

## Decision

**The surface `|` connects the left stage's stdout to the right stage's stdin;
neither endpoint must prove that it writes or reads.** Every interior edge is an
operating-system byte pipe allocated from stage position. A non-final stage's
returned value is discarded, while the final stage determines the pipeline's
value-boundary behaviour.

The typing rule imposes no adjacency mode:

```text
Γ ⊢ M : F[ρ] A       Γ ⊢ N : F[σ] B
────────────────────────────────────
          Γ ⊢ M | N : F[σ] B
```

This is not a value pipe. A returned `Bytes`, `String`, or structured value is
never serialised onto an interior edge. A value-returning stage which writes
nothing gives its successor EOF; a value-returning stage which writes stdout
gives its successor those bytes. The value itself goes nowhere.

## Why no endpoint contract

A byte stream may be empty, and a process may ignore its input. Opaque external
commands make both facts unknowable statically, while the runtime already routes
descriptors from position. Rejecting either shape therefore turns a usage hint
into a false typing judgment.

The payload route remains distinct for a different reason: at a value boundary
it selects the evaluator return or implicit stdout capture, and at a
process-staged pipeline boundary it selects whether the final helper reports a
value. It says nothing about whether a computation writes stdout and does not
participate in adjacency.

## Surface consequence

```ral
!{ return unit } | cat
# cat reads EOF

!{ echo hi; return unit } | from-string
# from-string returns "hi"

cat f | from-bytes | grep x
# the returned Bytes is discarded; grep reads EOF

echo hi | !{ return 5 }
# the final stage ignores stdin and returns 5
```

A whole-stage block literal remains a thunk and receives a syntax-directed
missing-`!` diagnostic. This is a CBPV syntax rule, not a channel contract.

## Relationship to the current rule

When implemented, this decision supersedes the endpoint-eligibility clause of
[[decisions/260809_byte-only-pipelines|byte-only-pipelines]] in full: neither the
producer payload nor the consumer input is pinned to `Bytes`. It retains that
decision's actual transport — byte-only operating-system pipes — and its refusal
of implicit value serialisation.

The implementation deletes `input`, deletes both pipeline adjacency checks,
and reduces per-stage mode annotations to the final-route information required
by capture and helper reporting. Until that change lands, the earlier active
decision continues to describe the checker.

See [[design/pipelines|pipelines]], [[design/capture|capture]], and
[[internals/pipeline-execution|pipeline execution]].
