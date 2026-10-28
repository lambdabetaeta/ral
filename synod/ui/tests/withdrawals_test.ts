/// <reference lib="deno.ns" />

import { withdrawals } from "../js/withdrawals.js";

function equal(actual: unknown, expected: unknown) {
  if (JSON.stringify(actual) !== JSON.stringify(expected)) {
    throw new Error(`expected ${JSON.stringify(expected)}; got ${JSON.stringify(actual)}`);
  }
}

function fixture() {
  let attempt = "a";
  let draft = "";
  const errors: unknown[] = [];
  const enqueue = withdrawals(
    () => attempt === "a",
    (prompts) => { draft = [...prompts, draft].filter(Boolean).join("\n\n"); },
    (error) => errors.push(error),
  );
  return {
    enqueue,
    errors,
    type(text: string) { draft = text; },
    restart() { attempt = "b"; },
    draft: () => draft,
  };
}

Deno.test("overlapping recalls restore A then B without losing the live draft", async () => {
  const f = fixture();
  const a = Promise.withResolvers<string[]>();
  const b = Promise.withResolvers<string[]>();
  const called: string[] = [];
  const first = f.enqueue(() => { called.push("a"); return a.promise; });
  const second = f.enqueue(() => { called.push("b"); return b.promise; });
  await Promise.resolve();
  equal(called, ["a"]);
  b.resolve(["B"]);
  f.type("typed while waiting");
  a.resolve(["A"]);
  await first;
  await second;
  equal(called, ["a", "b"]);
  equal(f.draft(), "A\n\nB\n\ntyped while waiting");
});

Deno.test("stop and recall share ordering and stop retains its gesture's exchange", async () => {
  for (const stopFirst of [false, true]) {
    const f = fixture();
    const barrier = Promise.withResolvers<string[]>();
    let exchange = 1;
    const target = exchange;
    const stopped: number[] = [];
    const stop = () => { stopped.push(target); return Promise.resolve(["stop"]); };
    const recall = () => barrier.promise;
    const first = f.enqueue(stopFirst ? stop : recall);
    const second = f.enqueue(stopFirst ? recall : stop);
    exchange = 2;
    f.type("draft");
    barrier.resolve(["recall"]);
    await first;
    await second;
    equal(stopped, [1]);
    equal(exchange, 2);
    equal(f.draft(), stopFirst ? "stop\n\nrecall\n\ndraft" : "recall\n\nstop\n\ndraft");
  }
});

Deno.test("superseded withdrawal responses cannot restore or start queued requests", async () => {
  const f = fixture();
  const response = Promise.withResolvers<string[]>();
  const first = f.enqueue(() => response.promise);
  let called = false;
  const second = f.enqueue(() => { called = true; return Promise.resolve(["queued"]); });
  await Promise.resolve();
  f.restart();
  f.type("new attempt draft");
  response.resolve(["old attempt text"]);
  await first;
  await second;
  equal(called, false);
  equal(f.draft(), "new attempt draft");
  equal(f.errors, []);
});

Deno.test("a failed withdrawal does not prevent later restoration; stale errors stay silent", async () => {
  for (const stale of [false, true]) {
    const f = fixture();
    const response = Promise.withResolvers<string[]>();
    const first = f.enqueue(() => response.promise);
    const second = f.enqueue(() => Promise.resolve(["B"]));
    await Promise.resolve();
    if (stale) f.restart();
    response.reject("withdrawal failed");
    await first;
    await second;
    equal(f.errors, stale ? [] : ["withdrawal failed"]);
    equal(f.draft(), stale ? "" : "B");
  }
});
