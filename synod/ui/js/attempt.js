/** @typedef {import("./bindings/ExchangeId.ts").ExchangeId} ExchangeId */
/** @typedef {{ attempt: string, phase: "starting" } | { attempt: string, phase: "ended" } | { attempt: string, phase: "accepted" | "ready", exchange: ExchangeId | null }} Attempt */
/** @typedef {{ attempt: string, type: "accepted" | "opening" | "ended" } | { attempt: string, type: "exchange_began" | "exchange_done", exchange: ExchangeId }} Action */

// Terminal attempts are absorbing; only addressed completions settle an exchange.
/** @param {Attempt} current @param {Action} action @returns {Attempt | null} */
export function advance(current, action) {
  if (current.attempt !== action.attempt || current.phase === "ended") return null;
  const attempt = current.attempt;
  switch (action.type) {
    case "accepted":
      return current.phase === "starting" ? { attempt, phase: "accepted", exchange: null } : null;
    case "opening":
      if (current.phase === "ready") return null;
      return { attempt, phase: "ready", exchange: current.phase === "accepted" ? current.exchange : null };
    case "exchange_began":
      return current.phase !== "starting" && current.exchange === null ? { ...current, exchange: action.exchange } : null;
    case "exchange_done":
      return current.phase !== "starting" && current.exchange === action.exchange ? { ...current, exchange: null } : null;
    case "ended":
      return { attempt, phase: "ended" };
    default: {
      /** @type {never} */
      const unhandled = action;
      return unhandled;
    }
  }
}
