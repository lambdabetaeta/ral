/// <reference lib="deno.ns" />

import { advance } from "../js/attempt.js";
import type { Attempt } from "../js/attempt.js";

function equal(actual: unknown, expected: unknown) {
  if (JSON.stringify(actual) !== JSON.stringify(expected)) {
    throw new Error(`expected ${JSON.stringify(expected)}; got ${JSON.stringify(actual)}`);
  }
}

Deno.test("completion settles without a stop promise acknowledgement", () => {
  const busy: Attempt = { attempt: "a", phase: "ready", job: 1 };
  const done = advance(busy, { attempt: "a", type: "job_done", job: 1 });
  equal(done, { attempt: "a", phase: "ready", job: null });
  if (!done) throw new Error("completion was ignored");
  const next = advance(done, { attempt: "a", type: "job_began", job: 2 });
  equal(next, { attempt: "a", phase: "ready", job: 2 });
  if (!next) throw new Error("next job was ignored");
  equal(advance(next, { attempt: "a", type: "job_done", job: 1 }), null);
});

Deno.test("superseded and duplicate completions cannot settle current work", () => {
  const busy: Attempt = { attempt: "b", phase: "ready", job: 1 };
  equal(advance(busy, { attempt: "a", type: "job_done", job: 1 }), null);
  const done = advance(busy, { attempt: "b", type: "job_done", job: 1 });
  if (!done) throw new Error("completion was ignored");
  equal(advance(done, { attempt: "b", type: "job_done", job: 1 }), null);
  const ended: Attempt = { attempt: "b", phase: "ended" };
  equal(advance(ended, { attempt: "b", type: "job_began", job: 2 }), null);
});
