/**
 * Serialize withdrawals; restore an overlapping batch in gesture order, before the live draft.
 * @param {() => boolean} current
 * @param {(prompts: string[]) => void} restore
 * @param {(error: unknown) => void} report
 */
export function withdrawals(current, restore, report) {
  let tail = Promise.resolve();
  let pending = 0;
  /** @type {string[]} */
  let prompts = [];

  /** @param {() => Promise<string[]>} withdraw */
  return function enqueue(withdraw) {
    pending++;
    tail = tail.then(async () => {
      try {
        if (current()) prompts.push(...await withdraw());
      } catch (error) {
        if (current()) report(error);
      } finally {
        pending--;
        if (pending === 0) {
          const batch = prompts;
          prompts = [];
          if (current() && batch.length) restore(batch);
        }
      }
    });
    return tail;
  };
}
