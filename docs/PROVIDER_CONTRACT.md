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

The input schema is vergerail.provider/1 (schemaVersion: 1). A model_turn
request contains provider-neutral messages, strict object tools, reasoning,
timeoutMs, and maximumResponseBytes. An image_generate request contains a
bounded prompt, reasoning, timeoutMs, and maximumResponseBytes. Unknown fields,
duplicate tools, non-object tool arguments, empty identifiers, oversized
frames, and invalid deadlines are rejected.

Successful model output is strict JSON with text, toolCalls, and usage. The
model is instructed to emit that object without markdown; the transport
deserializes with deny_unknown_fields and validates every returned tool against
the advertised set. Image output is exactly one completed PNG raster, returned
as a bounded base64 payload with byte length and dimensions. The decoded image
is limited to 8 MiB, 8192 pixels per side, and 8192² pixels. The app-server
JSONL frame cap is explicitly raised to 64 MiB for this provider, while
retained image data remains bounded to 8 MiB decoded.

Every operation uses Vergerail's read-only sandbox. Model turns additionally
use text-only feature gating. Image turns are audited before success and fail
if a command or file change appears. The provider ignores Codex's optional
savedPath; image bytes come only from the typed retained payload.

Timeouts, cancellation, transport disconnects, and any non-idempotent outcome
that cannot be observed are returned as typed non-retryable failures. The
caller owns recovery and must resolve an uncertain external image operation.
