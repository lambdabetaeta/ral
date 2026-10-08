---
status: accepted
generated_at_commit: 6a8d848f
verified_at_commit: 6a8d848f
anchors: [SignIn, host_id, finalize, login_flow, Granted, build_oauth_client, list_chatgpt, selection, shares_a_plan, UNWAITABLE, Meter]
---

# Sign in with ChatGPT token sharing

**exarch's `chatgpt` service speaks OpenAI's published route for open-source
apps: dynamic-client OAuth at `auth.openai.com`, inference at
`https://api.openai.com/v1/responses` with the plan's bearer token, and nothing
borrowed from the Codex CLI.**

## Context

exarch used to imitate the Codex CLI: its client id, `originator`, a
`codex_cli_rs` user-agent and `client_version`, `chatgpt-account-id` and
`openai-beta` headers, requests to `chatgpt.com/backend-api`, a device-code
flow, the `wham/usage` meter. OpenAI now publishes a token-sharing route for
open-source apps, and calls reuse of app-server auth by other services never
permitted. The imitation had no standing; the route does.

## Decision

- **One route.** `ORIGINATOR`, the user-agent, `client_version` and its
  `EXARCH_CODEX_CLIENT_VERSION` valve, the account and beta headers, the
  `backend-api/codex/models` lister, the `wham/usage` meter and the Codex-only
  `external_web_access` tool config are deleted. The resolver supplies the
  bearer; the endpoint is genai's default.
- **One flow.** OpenAI documents a loopback redirect to
  `http://127.0.0.1:<port>/auth/callback`; the device-code flow, `exarch login
  --device-auth` and the TUI's browser/device selector are deleted.
- **Host id and issued clients.** `oauth.json` holds one host id
  (`urn:uuid:<v4>`, minted once, never user-identifying) and, per account, the
  issued client id (`oaiapp_…`) and the retained ID token, so a re-login
  presents the account's own client and `id_token_hint`. The sign-in choice is
  `SignIn::Register` or `SignIn::Reauthorize(token)`; `exarch login <account>`
  renews.
- **The ID token is decoded, not verified.** It arrives from the issuer's
  token endpoint over TLS and names the account, nothing more. `finalize`
  checks that `nonce` matches the attempt, that `sub` is present (and equals
  the renewed account's), and that `chatgpt.tokens.use.direct` was granted.
- **No usage readout.** `chatgpt` is `Meter::Unpublished`. A spent cap is a
  429 `subscription_sharing_usage_limit_exceeded` naming no reset, in the
  unwaitable set, surfaced at once; the user is pointed at ChatGPT Settings →
  Usage.
- **No sampling controls.** The route refuses `temperature`, `top_p` and
  `max_output_tokens`; `Service::shares_a_plan` makes `Bureau`'s selection
  carry reasoning effort alone.
- **No migration.** Codex-era tokens were minted for another client and scope
  and are dead either way; a store of the old shape reads as no accounts.

## Consequences

`/limits` shows nothing for a plan. A full logout deletes the store and with it
the host id; the next sign-in mints another, which OpenAI sees as a new host.
Web search remains subject to the account's policy.

## Known gap

genai 0.7.0-rc.2 collapses a mid-stream `response.failed` to
`Error::StreamParse(message)`, losing `error.code`, so such a failure surfaces
as `ProviderError::Other` with the message. The fix is upstream: surface it as
`Error::ChatResponse { body }`, as the `error` event already is.

See [[map/exarch/provider|provider]] and
[[internals/provider-fault-recovery|provider-fault-recovery]].
