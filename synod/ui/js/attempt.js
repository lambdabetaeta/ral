/** @typedef {import("./bindings/JobId.ts").JobId} JobId */
/** @typedef {{ attempt: string, phase: "starting" } | { attempt: string, phase: "ended" } | { attempt: string, phase: "accepted" | "ready", job: JobId | null }} Attempt */
/** @typedef {{ attempt: string, type: "accepted" | "opening" | "ended" } | { attempt: string, type: "job_began" | "job_done", job: JobId }} Action */

// Terminal attempts are absorbing; only addressed completions settle a job.
/** @param {Attempt} current @param {Action} action @returns {Attempt | null} */
export function advance(current, action) {
  if (current.attempt !== action.attempt || current.phase === "ended") return null;
  const attempt = current.attempt;
  switch (action.type) {
    case "accepted":
      return current.phase === "starting" ? { attempt, phase: "accepted", job: null } : null;
    case "opening":
      if (current.phase === "ready") return null;
      return { attempt, phase: "ready", job: current.phase === "accepted" ? current.job : null };
    case "job_began":
      return current.phase !== "starting" && current.job === null ? { ...current, job: action.job } : null;
    case "job_done":
      return current.phase !== "starting" && current.job === action.job ? { ...current, job: null } : null;
    case "ended":
      return { attempt, phase: "ended" };
    default: {
      /** @type {never} */
      const unhandled = action;
      return unhandled;
    }
  }
}
