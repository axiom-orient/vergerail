# Vergerail provider transport contract

`vergerail-upagent-provider` is the versioned process boundary used by UpAgent. It
reads exactly one UTF-8 JSON request from stdin and writes exactly one JSON
response to stdout. It never reads an auth file or copies a token from disk. It
reuses the account state in the explicitly configured managed home; that home
must already be logged in. For an image request, the official app-server
deliberately exports short-lived auth to the trusted local Vergerail process via
`getAuthStatus(includeToken=true, refreshToken=true)`. The image adapter may
refresh that auth once after an observed HTTP 401. It does not replay a complete
provider operation. The pinned official runtime package, model, and workspace
are explicit environment configuration:

* `VERGERAIL_CODEX_PACKAGE`
* `VERGERAIL_CODEX_HOME` (an existing, already-authenticated managed Codex home)
* `VERGERAIL_MODEL`
* `VERGERAIL_WORKSPACE`

`VERGERAIL_CODEX_LOCK` and `VERGERAIL_CODEX_LOCK_SHA256` are not accepted. The
runtime identity is the embedded official lock; custom app-server builds and
patches are outside this contract.

The input schema is `vergerail.upagent/1` (`schemaVersion: 1`). A `model_turn`
request contains provider-neutral messages, strict object tools, reasoning,
`timeoutMs`, and `maximumResponseBytes`. An `image_generate` request contains a
bounded prompt, reasoning, timeoutMs, and maximumResponseBytes. It may also
contain the strict `imageOptions` object with the optional typed fields
`background` (`auto`, `transparent`, or `opaque`), `size` (`auto`, `1024x1024`,
`1536x1024`, or `1024x1536`), and `quality` (`auto`, `low`, `medium`, or
`high`). These options are sent directly by Vergerail's image adapter to the
fixed ChatGPT Images endpoint; prompt wording is never used as a control
fallback. Unknown fields, duplicate tools, non-object tool arguments, empty
identifiers, oversized frames, and invalid deadlines are rejected.

Reasoning is a clean six-value wire enum: `off`, `low`, `medium`, `high`,
`xhigh`, or `max`; omission is invalid. Values map directly to Vergerail's
native model-turn effort; there is no alias or downgrade. `image_generate`
keeps the field for `vergerail.upagent/1` envelope compatibility but bypasses
the model entirely, so model and reasoning do not influence image generation.
Model decoded text and image decoded bytes each have an 8 MiB caller cap. Model
JSON uses a 64 MiB encoded frame/retained-output cap to safely contain
worst-case JSON string escaping plus bounded tool calls. Image requests use a
distinct 16 MiB app-server/provider-output frame, with base64 and decoded
raster bounds that fit inside that frame; decoded image bytes remain capped at
8 MiB.

Successful model output is strict native JSON. With no advertised tools, the
model body contains only `text`; the provider returns an empty external
`toolCalls` vector. With tools, the native body contains `text` and a closed
`toolCalls` object with one required key per advertised tool. Each key maps to
an array of `{id, arguments}` objects, and unused tools use an empty array. The
provider deterministically flattens those arrays in advertised-tool order into
the external `toolCalls` vector and never asks the model to fabricate usage;
usage comes from the authoritative run result. The model is instructed to emit
the native object without markdown. `turn/start` receives an `outputSchema`
that propagates each advertised strict `inputSchema` directly into its tool
key; it uses no `oneOf`. Every nested object schema is recursively closed with
`additionalProperties: false`, while an explicitly open object schema is
rejected before session creation. Every object schema's `required` array is
also normalized to exactly its property keys; optional input properties are
represented as nullable values for native strict-schema compatibility. The
transport still deserializes with `deny_unknown_fields` and validates every
returned tool name, unique call ID, object arguments, per-call bytes, aggregate
bytes, and advertised set.

Image generation obtains an access token and bounded ChatGPT account claim
through the official app-server's `getAuthStatus` method with
`includeToken=true` and `refreshToken=true`, then sends one typed request to
`https://chatgpt.com/backend-api/codex/images/generations` with the official
runtime headers, `model`, `n=1`, `background`, `size`, and `quality`. Exactly one
valid PNG raster is success and is returned as a bounded base64 payload with
its actual byte length, dimensions, and alpha capability. A provider may return
dimensions or background treatment different from the requested preference;
this does not discard a valid image. The response reports observed dimensions
and optional `transparentBackground` metadata so UpAgent or PerfectPixel can
normalize it deterministically. Zero, multiple, malformed, or oversized images
still fail. The decoded image is limited to 8 MiB, 8192 pixels per side, and
8192² pixels.

HTTP 401 is treated as an authoritative authentication rejection for this
endpoint. It is the only condition that causes one auth reacquisition and one
retry, using the same logical image-turn correlation value; a second 401 is
terminal. The correlation header is not a standards-based idempotency guarantee,
so the provider never retries an unobserved or otherwise failed image operation.
The endpoint receives the remaining provider operation deadline rather than a
fixed timeout. Authentication is not propagated through a loopback MCP server:
trusted-origin gating makes that route unavailable and it is not part of this
architecture.

Model turns use Vergerail's read-only sandbox and text-only feature gating.
Direct image requests create no Codex thread, model turn, command, file change,
or saved path; image bytes come only from the typed endpoint response.

Typed failure messages are locally redacted for common credential markers and
bounded to 4 KiB; the complete failure envelope is bounded to 16 KiB, so an
oversized image or provider error cannot exceed the consumer frame.

Timeouts, cancellation, transport disconnects, and any non-idempotent outcome
that cannot be observed are returned as typed non-retryable failures. The
caller owns recovery and must resolve an uncertain external image operation.
Operation and shutdown/close failures are combined; cleanup failure never
replaces or hides the original operation failure.

## Official runtime configuration

Use the official pinned `0.150.1` package. No app-server source build, patch,
custom executable, or second runtime lock is part of this provider contract.
The provider receives the package and an existing authenticated home explicitly:

```bash
export VERGERAIL_CODEX_PACKAGE="/absolute/path/to/official-codex-package"
export VERGERAIL_CODEX_HOME="/absolute/path/to/already-authenticated-codex-home"
export VERGERAIL_MODEL="gpt-5.6-luna"
export VERGERAIL_WORKSPACE="/absolute/path/to/read-only-workspace"
```

On startup, Vergerail verifies the package against the embedded official lock
and starts its `app-server` mode. The image adapter uses only the app-server
auth RPC and the fixed endpoint; it never accesses the credential store. This
keeps image controls available without requiring an app-server rebuild on each
machine.
