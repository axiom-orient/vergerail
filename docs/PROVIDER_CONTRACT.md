# Vergerail provider transport contract

vergerail_provider is the versioned process boundary used by UpAgent. It
reads exactly one UTF-8 JSON request from stdin and writes exactly one JSON
response to stdout. It never reads an auth file, copies a token, or retries a
request. The managed home, pinned runtime package, owner, model, and workspace
are explicit environment configuration:

* VERGERAIL_CODEX_HOME
* VERGERAIL_CODEX_PACKAGE
* VERGERAIL_HOME_OWNER
* VERGERAIL_MODEL
* VERGERAIL_WORKSPACE

The input schema is `vergerail.provider/1` (`schemaVersion: 1`). A `model_turn`
request contains provider-neutral messages, strict object tools, reasoning,
`timeoutMs`, and `maximumResponseBytes`. An `image_generate` request contains a
bounded prompt, reasoning, timeoutMs, and maximumResponseBytes. Unknown fields,
duplicate tools, non-object tool arguments, empty identifiers, oversized
frames, and invalid deadlines are rejected.

Reasoning is a clean six-value wire enum: `off`, `low`, `medium`, `high`,
`xhigh`, or `max`; omission is invalid. Values map directly to Vergerail's
native turn effort; there is no alias or downgrade. Model decoded text and
image decoded bytes each have an 8 MiB caller cap. Model JSON uses a 64 MiB
encoded frame/retained-output cap to safely contain worst-case JSON string
escaping plus bounded tool calls. Image turns use a distinct 16 MiB
app-server/provider-output frame, with base64 and decoded raster bounds that
fit inside that frame; decoded image bytes remain capped at 8 MiB.

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
bytes, and advertised set. Image output is exactly one completed PNG raster,
returned as a bounded base64 payload with byte length and dimensions. The
decoded image is limited to 8 MiB, 8192 pixels per side, and 8192² pixels.

Every operation uses Vergerail's read-only sandbox. Model turns additionally
use text-only feature gating. Image turns enable only image generation and are audited before success and fail
if a command or file change appears. The provider ignores Codex's optional
savedPath; image bytes come only from the typed retained payload.

Typed failure messages are locally redacted for common credential markers and
bounded to 4 KiB; the complete failure envelope is bounded to 16 KiB, so an
oversized image or provider error cannot exceed the consumer frame.

Timeouts, cancellation, transport disconnects, and any non-idempotent outcome
that cannot be observed are returned as typed non-retryable failures. The
caller owns recovery and must resolve an uncertain external image operation.
Operation and shutdown/close failures are combined; cleanup failure never
replaces or hides the original operation failure.
