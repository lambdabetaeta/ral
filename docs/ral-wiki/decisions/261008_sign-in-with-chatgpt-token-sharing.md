---
status: accepted
generated_at_commit: dd8a5443b8f87dc40de66bcf5c5a72df05a723d8
verified_at_commit: dd8a5443b8f87dc40de66bcf5c5a72df05a723d8
anchors: [Login, Grant, SignIn, Method, methods, OAuthToken, Revocation, host_id, login_flow, build_oauth_client, list_login, plan_models, shares_a_plan, UNWAITABLE, Meter]
---

# Sign in with ChatGPT token sharing

**`openai-chatgpt` speaks OpenAI's published token-sharing route for open-source
apps: dynamic-client OAuth at `auth.openai.com` and inference at
`https://api.openai.com/v1/responses`. `openai-codex` and `xai-grok` are separate,
explicit sign-ins, not extensions of that policy or protocol.**

## Context

exarch used to imitate the Codex CLI: its client id, `originator`, a
`codex_cli_rs` user-agent and `client_version`, `chatgpt-account-id` and
`openai-beta` headers, requests to `chatgpt.com/backend-api`, a device-code
flow, the `wham/usage` meter. OpenAI now publishes a token-sharing route for
open-source apps, and calls reuse of app-server auth by other services never
permitted. The imitation had no standing; the route does.

## Decision

- **One token-sharing route.** Only `openai-chatgpt` uses the issued dynamic
  client, stock Responses endpoint and bearer without Codex-specific headers.
  `openai-codex` uses the Codex public client, backend Responses endpoint,
  account/beta/originator/user-agent headers, versioned model catalog and
  `wham/usage` meter. `xai-grok` uses pi's xAI public client and genai's Xai
  adapter. **Public-client reuse is not issuer authorization for exarch.**
  OpenAI's stated restriction on other services reusing app-server auth remains
  a policy caveat; the existence of these paths does not establish approval.
- **Explicit methods.** `Login::{Chatgpt, Codex, Grok}` names the service;
  `methods` offers browser only for Chatgpt, browser/device for Codex, device
  only for Grok. `SignIn::New(login, method)` adds an account;
  `SignIn::Renew(token, method)` renews the selected identity, refusing a
  different returned account rather than replacing it.
- **Host id and issued clients.** `oauth.json` holds one host id
  (`urn:uuid:<v4>`, minted once, never user-identifying), used only by Chatgpt,
  and that grant retains its issued client id and ID token. Renewal presents
  that account's own client and `id_token_hint`.
- **The ID token is decoded, not verified.** It arrives from the issuer's
  token endpoint over TLS and names the account, nothing more. Chatgpt checks
  that `nonce` matches the attempt, `sub` is present (and equals the renewed
  account's), and `chatgpt.tokens.use.direct` was granted.
- **Route-specific allowance and tuning.** `openai-chatgpt` and `xai-grok`
  carry `Meter::Unpublished`; ChatGPT token-sharing usage is in Settings →
  Usage. Its `subscription_sharing_usage_limit_exceeded` is unwaitable;
  Codex's `usage_limit_reached` remains an allowance refusal with a reset.
  `Meter::Codex` reads server-defined windows, not hard-coded durations.
  Only `Service::shares_a_plan` for Chatgpt removes sampling and output caps;
  Codex/Grok retain full tuning and `max_tokens`.
- **Identity is not email.** Chatgpt and Grok key by returned subject; no
  independent Chatgpt workspaces are promised unless subjects distinguish them.
  Codex keys by `base64url(sub):base64url(chatgpt_account_id)`, distinguishing
  both people in one workspace and one person in two workspaces. The raw
  backend account id is retained separately for request headers. Workspace,
  plan and email only name an account. Service-qualified keys never alias.
- **One token sum and store boundary.** `OAuthToken` owns the common fields;
  `Grant::{Chatgpt, Codex, Grok}` owns route-specific data. The flat stored
  entry names its service. Unknown services, missing semantic credentials and
  mismatched keys reject only that entry with a non-secret warning. Invalid
  JSON or structurally undecodable required fields make the whole store
  unreadable, and writers never overwrite it. Atomic owner-private writes
  remain through `OwnDir` and `Visibility::Secret`.
- **No migration.** Old `chatgpt:*` entries are rejected with warnings, not
  renamed or imported. Sign in again; stale cache keys do not alias new
  accounts, and stale selection takes the normal unavailable/picker path.

## Consequences and live acceptance

Each account has independent selection, refresh, renewal and logout. Codex
logout is **local-only** (`Revocation::LocalOnly`): removed refresh credentials
may remain usable at the issuer. Chatgpt/Grok attempt issuer revocation
(`Revocation::AtIssuer` on acceptance); even accepted revocation need not
immediately invalidate access tokens. Issuer-side session/app removal is separate.

No live authentication or inference is established by this implementation.
Grok initially requires an ID token with nonempty `sub`; discovery advertises
OIDC, but that actual device response is unverified. Missing identity fails
closed without saving; there is no speculative userinfo/email fallback.
Grok bearer compatibility with **both** genai Xai chat-completions inference
and `/v1/models` is unverified: pi's Responses path and static catalog prove
neither. A live failure may need a different adapter, endpoint or listing path.
Devday protocol bugs are outside this change. Synod's window still offers only
ChatGPT sign-in; other accounts can be added through exarch.

A later user-led exercise can add two identities per route, pin their exact
ids, renew/refresh/logout one without affecting the other, exercise Codex
browser/device, raw account headers, version floor, search and `/limits`, and
confirm Grok ID-token subject presence, model listing and a tool-capable Xai
request. Also exercise wrong-account renewal, cancellation and denied/expired
device codes. Do not copy tokens, JWTs, device codes or authorization URLs into
logs, chat or this wiki.

## Known gap

genai 0.7.0-rc.8 still collapses a mid-stream `response.failed` to
`Error::StreamParse(message)`, losing `error.code`, so such a failure surfaces
as `ProviderError::Other` with the message. The fix is upstream: surface it as
`Error::ChatResponse { body }`, as the `error` event already is.

See [[map/exarch/provider|provider]] and
[[internals/provider-fault-recovery|provider-fault-recovery]].
