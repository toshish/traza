// Span-shape helpers shared by the views.

/** Tree order for the waterfall: roots by start time, children nested under
    parents (DFS), each with its depth. Orphaned parents and cycles fall back
    to root level so a malformed trace still renders every span. */
export function waterfallOrder(spans) {
  const sorted = [...spans].sort((a, b) =>
    a.start_time_ns - b.start_time_ns || a.end_time_ns - b.end_time_ns || (a.span_id < b.span_id ? -1 : 1));
  const byId = new Map(sorted.map((s) => [s.span_id, s]));
  const children = new Map();
  const roots = [];
  for (const span of sorted) {
    const parent = span.parent_span_id;
    if (parent && parent !== span.span_id && byId.has(parent)) {
      if (!children.has(parent)) children.set(parent, []);
      children.get(parent).push(span);
    } else {
      roots.push(span);
    }
  }
  const out = [];
  const visited = new Set();
  const visit = (span, depth) => {
    if (visited.has(span.span_id)) return;
    visited.add(span.span_id);
    out.push({ span, depth });
    for (const child of children.get(span.span_id) || []) visit(child, depth + 1);
  };
  roots.forEach((root) => visit(root, 0));
  // Cycles unreachable from any root still belong on screen.
  sorted.forEach((span) => visit(span, 0));
  return out;
}

/** True when an attribute/event value is an offloaded-payload reference:
    {"$payload": "sha256/…", "bytes": N, "preview": "…"}. */
/** Time a span spent in itself rather than waiting on its children.

    A waterfall shows where time went; without self time it only shows where
    time was, and a 8s root that spent 7.9s in one child looks identical to
    one that spent 8s of its own. Overlapping children are unioned rather than
    summed — concurrent tool calls are the normal case in an agent trace, and
    summing them would report negative self time. */
export function selfTimeNs(span, children) {
  const total = span.end_time_ns - span.start_time_ns;
  if (!children || !children.length) return total;
  const intervals = children
    .map((child) => [child.start_time_ns, child.end_time_ns])
    .sort((a, b) => a[0] - b[0]);
  let covered = 0;
  let [start, end] = intervals[0];
  for (const [from, to] of intervals.slice(1)) {
    if (from > end) { covered += end - start; [start, end] = [from, to]; }
    else if (to > end) end = to;
  }
  covered += end - start;
  return Math.max(0, total - covered);
}

/** The chain of spans that determines the trace's duration.

    At each level the critical path follows the child that finishes last:
    shortening anything off this path cannot make the trace faster, which is
    the only thing worth knowing when a run is too slow. */
export function criticalPath(spans) {
  if (!spans.length) return new Set();
  const byParent = new Map();
  for (const span of spans) {
    const key = span.parent_span_id || '';
    if (!byParent.has(key)) byParent.set(key, []);
    byParent.get(key).push(span);
  }
  const roots = spans.filter((s) => !s.parent_span_id || !spans.some((o) => o.span_id === s.parent_span_id));
  const path = new Set();
  let current = roots.sort((a, b) => (b.end_time_ns - b.start_time_ns) - (a.end_time_ns - a.start_time_ns))[0];
  while (current) {
    path.add(current.span_id);
    const children = byParent.get(current.span_id) || [];
    current = children.sort((a, b) => b.end_time_ns - a.end_time_ns)[0];
  }
  return path;
}

/** Children of each span, keyed by parent span id. */
export function childrenOf(spans) {
  const map = new Map();
  for (const span of spans) {
    if (!span.parent_span_id) continue;
    if (!map.has(span.parent_span_id)) map.set(span.parent_span_id, []);
    map.get(span.parent_span_id).push(span);
  }
  return map;
}

export function isPayloadRef(value) {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
    && typeof value.$payload === 'string';
}

/** Every payload reference in a span (attributes and event attributes). */
export function collectPayloadRefs(span) {
  const refs = [];
  const scan = (obj, where) => {
    if (!obj || typeof obj !== 'object') return;
    for (const [key, value] of Object.entries(obj)) {
      if (isPayloadRef(value)) refs.push({ where, key, ref: value.$payload, bytes: value.bytes, preview: value.preview });
    }
  };
  scan(span.attributes, 'attributes');
  (span.events || []).forEach((event) => scan(event.attributes, 'event ' + event.name));
  return refs;
}

// Semantic-convention key precedence, mirroring src/semconv.rs (the source of
// truth on the server). Traza recognizes the OTel GenAI and OpenLLMetry
// conventions (gen_ai.*, llm.usage.*, traceloop.*), OpenInference
// (openinference.span.kind, llm.model_name, llm.token_count.*, tool.name,
// agent.name, input.value / output.value) and its own native llm.* /
// session.id shorthand; keep these lists in step with the Rust module.
const SESSION_KEYS = [
  'session.id',
  'gen_ai.conversation.id',
  'traceloop.association.properties.session_id',
  'traceloop.association.properties.chat_id',
];

function firstNum(attrs, keys) {
  for (const key of keys) {
    const v = attrs[key];
    if (typeof v === 'number') return v;
    if (typeof v === 'string' && v.trim() !== '' && Number.isFinite(Number(v))) return Number(v);
  }
  return null;
}

function firstStr(attrs, keys) {
  for (const key of keys) {
    const v = attrs[key];
    if (typeof v === 'string' && v) return v;
    if (typeof v === 'number') return String(v);
  }
  return null;
}

/** LLM usage figures when the span carries recognized LLM attributes, else
    null. Resolves the OTel GenAI conventions, OpenLLMetry, OpenInference and
    Traza's native llm.* shorthand in the order src/semconv.rs `facts()` does
    — current OTel names first, the deprecated names OTel replaced as
    aliases, OpenInference's `llm.model_name` / `llm.token_count.*` last. */
export function llmUsage(span) {
  const attrs = span.attributes || {};
  const model = firstStr(attrs, ['gen_ai.response.model', 'gen_ai.request.model', 'llm.model', 'llm.model_name']);
  const provider = firstStr(attrs, ['gen_ai.provider.name', 'gen_ai.system', 'llm.provider', 'llm.system']);
  const promptTokens = firstNum(attrs, ['gen_ai.usage.input_tokens', 'gen_ai.usage.prompt_tokens', 'llm.prompt_tokens', 'llm.token_count.prompt']);
  const completionTokens = firstNum(attrs, ['gen_ai.usage.output_tokens', 'gen_ai.usage.completion_tokens', 'llm.completion_tokens', 'llm.token_count.completion']);
  const explicitTotal = firstNum(attrs, ['llm.usage.total_tokens', 'gen_ai.usage.total_tokens', 'llm.total_tokens', 'llm.token_count.total']);
  const costUsd = firstNum(attrs, ['llm.cost_usd', 'gen_ai.usage.cost']);
  const cacheReadTokens = firstNum(attrs, ['gen_ai.usage.cache_read.input_tokens', 'gen_ai.usage.cache_read_input_tokens']);
  const cacheCreationTokens = firstNum(attrs, ['gen_ai.usage.cache_creation.input_tokens', 'gen_ai.usage.cache_creation_input_tokens']);
  const stopReason = firstStr(attrs, ['gen_ai.response.finish_reason', 'gen_ai.response.stop_reason', 'llm.stop_reason']);
  const operation = firstStr(attrs, ['gen_ai.operation.name', 'llm.request.type']);
  const spanKind = lower(firstStr(attrs, ['traceloop.span.kind']));
  const oiKind = lower(firstStr(attrs, ['openinference.span.kind']));
  const anything = model != null || provider != null || promptTokens != null || completionTokens != null
    || explicitTotal != null || costUsd != null || stopReason != null || operation != null
    || spanKind === 'llm' || oiKind === 'llm';
  if (!anything) return null;
  const totalTokens = explicitTotal != null ? explicitTotal
    : (promptTokens != null || completionTokens != null) ? (promptTokens || 0) + (completionTokens || 0) : null;
  return {
    model, provider, promptTokens, completionTokens, totalTokens, costUsd, stopReason,
    cacheReadTokens, cacheCreationTokens,
  };
}

function lower(value) { return typeof value === 'string' ? value.toLowerCase() : value; }

/** What kind of step a span is, by the precedence of src/semconv.rs
    `step_facts()`: `gen_ai.operation.name`, then `openinference.span.kind`,
    then `traceloop.span.kind`, then "it looks like an LLM call", then a tool
    name. An unrecognized value falls through to the next source. Returns
    `llm`, `tool`, `agent`, `retrieval`, `embedding` or `other`. */
export function stepKindOf(span) {
  const attrs = span.attributes || {};
  const operation = lower(firstStr(attrs, ['gen_ai.operation.name']));
  const byOperation = {
    chat: 'llm', text_completion: 'llm', generate_content: 'llm', execute_tool: 'tool',
    invoke_agent: 'agent', create_agent: 'agent', embeddings: 'embedding',
  }[operation];
  if (byOperation) return byOperation;
  const oi = lower(firstStr(attrs, ['openinference.span.kind']));
  const byOi = { llm: 'llm', tool: 'tool', agent: 'agent', retriever: 'retrieval', embedding: 'embedding', chain: 'other' }[oi];
  if (byOi) return byOi;
  const traceloop = lower(firstStr(attrs, ['traceloop.span.kind']));
  const byTraceloop = { llm: 'llm', tool: 'tool', agent: 'agent', workflow: 'other', task: 'other' }[traceloop];
  if (byTraceloop) return byTraceloop;
  if (llmUsage(span)) return 'llm';
  if (firstStr(attrs, ['gen_ai.tool.name', 'tool.name'])) return 'tool';
  return 'other';
}

/** Whether a span is a STEP of its run (a model call, a tool call, or an
    agent invocation) — what a tape, a step count and repeat detection are
    made of. Retrievals, embeddings and wrappers are context, not steps. */
export function isStep(span) {
  const kind = stepKindOf(span);
  return kind === 'llm' || kind === 'tool' || kind === 'agent';
}

/** The tool and agent a span names, by the semconv.rs names table:
    tool `gen_ai.tool.name` → `tool.name` → `traceloop.entity.name` (when the
    Traceloop kind is `tool`); agent `gen_ai.agent.name` →
    `traceloop.entity.name` (kind `agent` or `workflow`) → `agent.name`. */
export function stepNamesOf(span) {
  const attrs = span.attributes || {};
  const traceloop = lower(firstStr(attrs, ['traceloop.span.kind']));
  const tool = firstStr(attrs, ['gen_ai.tool.name', 'tool.name'])
    || (traceloop === 'tool' ? firstStr(attrs, ['traceloop.entity.name']) : null);
  const agent = firstStr(attrs, ['gen_ai.agent.name'])
    || ((traceloop === 'agent' || traceloop === 'workflow') ? firstStr(attrs, ['traceloop.entity.name']) : null)
    || firstStr(attrs, ['agent.name']);
  return { tool: tool || null, agent: agent || null };
}

/** The run a span belongs to: its session id, else `trace:<trace_id>`
    (src/semconv.rs `run_id`). Membership is per span, as it is there. */
export function runIdOf(span) {
  const session = sessionIdOf(span);
  return session ? session.id : 'trace:' + span.trace_id;
}

/** The span's session id and the attribute key carrying it, or null. Mirrors
    the session-key precedence in src/semconv.rs. */
export function sessionIdOf(span) {
  const attrs = span.attributes || {};
  for (const key of SESSION_KEYS) {
    const v = attrs[key];
    const id = typeof v === 'string' && v ? v : (typeof v === 'number' ? String(v) : null);
    if (id) return { id, key };
  }
  return null;
}

// Very long text is clipped for display; the full value stays in Attributes
// and, when offloaded, behind the payload fetch.
const MAX_TEXT_CHARS = 20000;

function clip(text, limit) {
  return text.length > limit ? text.slice(0, limit) + '\n… (' + text.length + ' chars total)' : text;
}

/** Human byte label. */
export function bytesLabel(bytes) {
  if (typeof bytes !== 'number' || !Number.isFinite(bytes)) return null;
  if (bytes >= 1 << 20) return (bytes / (1 << 20)).toFixed(1) + ' MiB';
  if (bytes >= 1 << 10) return (bytes / (1 << 10)).toFixed(1) + ' KiB';
  return bytes + ' B';
}

const MEDIA_KINDS = ['image', 'audio', 'video', 'document', 'file'];

/** Media kind for a part type or MIME type, else null. */
function mediaKindOf(type, mime) {
  if (typeof type === 'string') {
    const t = type.toLowerCase();
    if (t === 'file') return 'document';
    if (MEDIA_KINDS.includes(t)) return t;
    if (t === 'image_url' || t === 'input_image') return 'image';
    if (t === 'input_audio' || t === 'output_audio') return 'audio';
  }
  if (typeof mime === 'string') {
    if (mime.startsWith('image/')) return 'image';
    if (mime.startsWith('audio/')) return 'audio';
    if (mime.startsWith('video/')) return 'video';
    if (mime === 'application/pdf' || mime.startsWith('text/')) return 'document';
  }
  return null;
}

/** A browser can only render data: and http(s): sources; s3://, gs:// and
    friends are real locators but not fetchable here, so they are shown as
    references rather than broken media. */
function renderableSrc(value) {
  if (typeof value !== 'string') return null;
  return /^(data:|https?:)/i.test(value) ? value : null;
}

// Bare base64 payloads (the OTel GenAI `data` field, Anthropic `source.data`,
// Google `inline_data.data`, tool screenshots…) carry no scheme, so the
// browser cannot use them as-is; with the MIME type they become a data: URI.
// The length floor keeps ordinary words ("summary") from looking like bytes.
export function base64ToDataUri(value, mime) {
  if (typeof value !== 'string' || value.length < 16) return null;
  const clean = /\s/.test(value) ? value.replace(/\s+/g, '') : value;
  if (clean.length % 4 !== 0 || !/^[A-Za-z0-9+/]+={0,2}$/.test(clean)) return null;
  return 'data:' + (mime || 'application/octet-stream') + ';base64,' + clean;
}

// A reference worth SHOWING is scheme-shaped and short (a URI or an id) — an
// opaque blob that fails both the URI and base64 tests must not be poured
// into the transcript as text, which is how a screenshot once rendered as
// eight thousand lines of base64.
function referenceUri(value) {
  if (typeof value !== 'string' || !value || value.length > 2048) return null;
  if (/^data:/i.test(value)) return null; // bytes, not a reference
  return /^[a-z][a-z0-9+.-]*:/i.test(value) ? value : null;
}

// MIME for the OpenAI input_audio {data, format} shape.
function audioFormatMime(format) {
  if (typeof format !== 'string' || !format) return undefined;
  const f = format.toLowerCase();
  return f === 'mp3' ? 'audio/mpeg' : 'audio/' + f;
}

/** Media descriptor for one message part across the shapes emitters actually
    produce, else null. Recognized, in rough order of the wild population:
      - OTel GenAI / Traza native: {type, mime_type, data|uri, filename, …}
      - OpenAI: {type:"image_url", image_url:{url}|url}, {type:"input_audio",
        input_audio:{data, format}}, {type:"file", file:{file_data|file_id}}
      - Anthropic: {type, source:{type:"base64"|"url"|"file", data|url|file_id}}
      - Google GenAI: {inline_data|inlineData:{mime_type, data}},
        {file_data|fileData:{mime_type, file_uri}} — typeless parts
    Bytes render (data:/http(s), or bare base64 lifted into a data: URI);
    object-store locators stay references; offloaded bytes carry their
    payload ref so the renderer can fetch them on demand. */
function mediaPartOf(part, type) {
  const source = typeof part.source === 'object' && part.source !== null ? part.source : undefined;
  const inline = part.inline_data ?? part.inlineData;
  const fileRef = part.file_data ?? part.fileData;
  const audioIn = typeof part.input_audio === 'object' && part.input_audio !== null ? part.input_audio : undefined;
  const fileBox = typeof part.file === 'object' && part.file !== null ? part.file : undefined;

  const mime = part.mime_type || part.mimeType || part.media_type
    || (source && source.media_type)
    || (inline && typeof inline === 'object' ? inline.mime_type || inline.mimeType : undefined)
    || (fileRef && typeof fileRef === 'object' ? fileRef.mime_type || fileRef.mimeType : undefined)
    || (fileBox && fileBox.mime_type)
    || (audioIn && audioFormatMime(audioIn.format));

  // Typeless Google parts are media by virtue of carrying inline or file
  // data; a document is the honest default when even the MIME is missing.
  const media = mediaKindOf(type, mime)
    || ((inline != null || fileRef != null) ? (mediaKindOf(undefined, mime) || 'document') : null)
    || ((type === undefined && (audioIn || fileBox)) ? (audioIn ? 'audio' : 'document') : null);
  if (!media) return null;

  // Locator candidates, most specific first. Each may be a string (bytes or
  // URI), an object with .url (OpenAI image_url), or a payload reference.
  const candidates = [
    part.data, source && source.data, inline && typeof inline === 'object' ? inline.data : inline,
    audioIn && audioIn.data, fileBox && fileBox.file_data,
    part.uri, part.url, source && source.url,
    fileRef && typeof fileRef === 'object' ? (fileRef.file_uri ?? fileRef.fileUri) : fileRef,
    part.image_url, fileBox && fileBox.file_id, source && source.file_id, part.file_id,
  ];

  let src = null;
  let uri;
  let payloadRef;
  for (const raw of candidates) {
    if (raw == null) continue;
    if (isPayloadRef(raw)) {
      if (!payloadRef) payloadRef = { ref: raw.$payload, bytes: raw.bytes };
      continue;
    }
    const value = typeof raw === 'object' ? raw.url : raw;
    if (typeof value !== 'string' || !value) continue;
    src = renderableSrc(value) || base64ToDataUri(value, mime);
    if (src) break;
    if (uri === undefined) uri = referenceUri(value) || undefined;
  }

  const unavailable = part.capture_status === 'unavailable' || part.archive_status === 'unavailable'
    || part.status === 'unavailable';
  return {
    kind: 'media',
    mediaType: media,
    mime: mime || undefined,
    filename: part.filename || part.name || (fileBox && fileBox.filename) || undefined,
    sizeBytes: typeof part.size_bytes === 'number' ? part.size_bytes : undefined,
    width: typeof part.width === 'number' ? part.width : undefined,
    height: typeof part.height === 'number' ? part.height : undefined,
    src,
    uri: src ? undefined : uri,
    payloadRef: src ? undefined : payloadRef,
    unavailable: unavailable || undefined,
    unavailableReason: (unavailable && (part.unavailable_reason || part.reason || part.detail)) || undefined,
  };
}

// One part of a message, normalized into a shape the renderer can switch on.
export function normalizePart(part) {
  if (part == null) return { kind: 'text', text: '' };
  if (typeof part !== 'object') return { kind: 'text', text: String(part) };
  if (isPayloadRef(part)) {
    return { kind: 'payload', ref: part.$payload, bytes: part.bytes, preview: part.preview || '' };
  }
  if (isPayloadRef(part.content)) {
    return { kind: 'payload', ref: part.content.$payload, bytes: part.content.bytes, preview: part.content.preview || '' };
  }
  const type = typeof part.type === 'string' ? part.type.toLowerCase() : undefined;
  if (type === 'tool_call') {
    return { kind: 'tool_call', id: part.id, name: part.name || '', args: part.arguments ?? part.args ?? {} };
  }
  if (type === 'tool_call_response' || type === 'tool_result') {
    return { kind: 'tool_result', id: part.id, result: part.result ?? part.response ?? part.content ?? '' };
  }
  const media = mediaPartOf(part, type);
  if (media) return media;
  if (typeof part.content === 'string') return { kind: 'text', text: clip(part.content, MAX_TEXT_CHARS) };
  if (typeof part.text === 'string') return { kind: 'text', text: clip(part.text, MAX_TEXT_CHARS) };
  return { kind: 'text', text: clip(JSON.stringify(part, null, 2), MAX_TEXT_CHARS) };
}

/** Message parts hiding inside a tool result: MCP and computer-use tools
    answer with a content list whose entries are ordinary parts, screenshots
    included. Returns normalized parts when `value` holds at least one media
    part, else null — a plain JSON result is better shown as JSON. */
export function toolResultParts(value) {
  const list = Array.isArray(value) ? value
    : (value && typeof value === 'object' && Array.isArray(value.content)) ? value.content : null;
  if (!list || !list.length) return null;
  const parts = list.map(normalizePart);
  return parts.some((p) => p.kind === 'media') ? parts : null;
}

// One message -> { role, parts: [...] }. Accepts the OTel GenAI shape
// ({role, parts:[...]}), the older {role, content} shape, and a bare string.
function normalizeMessage(message, direction) {
  if (message == null) return { direction, role: undefined, parts: [] };
  if (typeof message === 'string') {
    return { direction, role: undefined, parts: [{ kind: 'text', text: clip(message, MAX_TEXT_CHARS) }] };
  }
  if (isPayloadRef(message)) return { direction, role: undefined, parts: [normalizePart(message)] };
  const role = typeof message.role === 'string' ? message.role : undefined;
  const finishReason = message.finish_reason || message.finishReason;
  if (Array.isArray(message.parts)) {
    return { direction, role, finishReason, parts: message.parts.map(normalizePart) };
  }
  if (Array.isArray(message.content)) {
    return { direction, role, finishReason, parts: message.content.map(normalizePart) };
  }
  if (message.content != null) {
    return { direction, role, finishReason, parts: [normalizePart({ content: message.content })] };
  }
  return { direction, role, finishReason, parts: [normalizePart(message)] };
}

// Parses a gen_ai.{input,output}.messages attribute, which OpenLLMetry emits
// JSON-encoded (a string) but native ingest may carry as an array. A whole
// attribute past the offload threshold arrives as a {$payload} reference —
// surface it instead of silently rendering nothing, and mark it as a whole
// offloaded CONVERSATION (`offloadedMessages`) so the renderer knows the
// payload body parses back into messages rather than into prose.
function parseMessagesAttr(value, direction) {
  if (value == null) return [];
  if (isPayloadRef(value)) {
    return [{ direction, role: undefined, offloadedMessages: true, parts: [normalizePart(value)] }];
  }
  let parsed = value;
  if (typeof value === 'string') {
    try { parsed = JSON.parse(value); } catch (e) {
      return [{ direction, role: undefined, parts: [{ kind: 'text', text: clip(value, MAX_TEXT_CHARS) }] }];
    }
  }
  const list = Array.isArray(parsed) ? parsed : [parsed];
  return list.map((m) => normalizeMessage(m, direction));
}

/** Messages reassembled from an indexed layout — OpenLLMetry's
    `gen_ai.prompt.{i}.{role,content,…}` or OpenInference's
    `llm.input_messages.{i}.message.{role,content,…}` — ordered by numeric
    index, with tool calls folded into tool_call parts the way semconv.rs
    folds them (`{i}.tool_calls.{j}.{id,name,arguments}`, and OpenInference's
    `{i}.message.tool_calls.{j}.tool_call.{id,function.name,function.arguments}`). */
function indexedMessages(attrs, prefix, inner, direction) {
  const byIndex = new Map();
  for (const [key, value] of Object.entries(attrs)) {
    if (!key.startsWith(prefix)) continue;
    let rest = key.slice(prefix.length);
    const dot = rest.indexOf('.');
    if (dot < 0) continue;
    const index = rest.slice(0, dot);
    // An index with a leading zero is not an index (semconv.rs ignores it).
    if (!/^(0|[1-9]\d*)$/.test(index)) continue;
    rest = rest.slice(dot + 1);
    if (inner) {
      if (!rest.startsWith(inner)) continue;
      rest = rest.slice(inner.length);
    }
    if (!byIndex.has(index)) byIndex.set(index, { calls: new Map() });
    const message = byIndex.get(index);
    if (rest === 'role' || rest === 'content' || rest === 'finish_reason' || rest === 'tool_call_id') {
      message[rest] = value;
      continue;
    }
    const call = /^tool_calls\.(\d+)\.(.+)$/.exec(rest);
    if (!call) continue;
    const field = {
      id: 'id', name: 'name', arguments: 'arguments', 'tool_call.id': 'id',
      'tool_call.function.name': 'name', 'tool_call.function.arguments': 'arguments',
    }[call[2]];
    if (!field) continue;
    if (!message.calls.has(call[1])) message.calls.set(call[1], {});
    message.calls.get(call[1])[field] = value;
  }
  return [...byIndex.entries()]
    .sort((a, b) => Number(a[0]) - Number(b[0]))
    .map(([, m]) => {
      const parts = [];
      if (m.content != null) parts.push(normalizePart({ content: m.content }));
      [...m.calls.entries()].sort((a, b) => Number(a[0]) - Number(b[0])).forEach(([, c]) => {
        parts.push({ kind: 'tool_call', id: c.id, name: c.name || '', args: parseMaybeJson(c.arguments ?? {}) });
      });
      if (!parts.length) parts.push(normalizePart({ content: '' }));
      return { direction, role: m.role, finishReason: m.finish_reason, parts };
    });
}

function parseMaybeJson(value) {
  if (typeof value !== 'string') return value;
  const trimmed = value.trim();
  if (!trimmed.startsWith('{') && !trimmed.startsWith('[')) return value;
  try { return JSON.parse(trimmed); } catch (e) { return value; }
}

/** One plain value (`input.value`, `llm.prompt`) as messages: a JSON list
    of `{role, …}` objects reads as that conversation, anything else as one
    message. */
function valueMessage(value, direction) {
  if (value == null || value === '') return [];
  if (isPayloadRef(value)) return [{ direction, role: undefined, parts: [normalizePart(value)] }];
  if (typeof value === 'string') {
    const parsed = parseMaybeJson(value);
    if (Array.isArray(parsed) && parsed.length && parsed.every((m) => m && typeof m === 'object' && 'role' in m)) {
      return parsed.map((m) => normalizeMessage(m, direction));
    }
    return [normalizeMessage(value, direction)];
  }
  return [normalizeMessage({ content: value }, direction)];
}

/** Chat turns recovered from an LLM span, in order, as structured messages:
    { direction: 'prompt'|'completion', role, finishReason, parts: [...] }.
    Part kinds are 'text', 'media', 'tool_call', 'tool_result', 'payload'.

    Each side takes the first source present, by the precedence of
    src/semconv.rs: JSON `gen_ai.input.messages` / `gen_ai.output.messages`
    ({role, parts:[{type,content}]}); then OpenLLMetry's indexed
    `gen_ai.prompt.{i}.*` / `gen_ai.completion.{i}.*`; then OpenInference's
    indexed `llm.input_messages.{i}.message.*` / `llm.output_messages.*`; then
    `input.value` / `output.value`; then a native `llm.prompt` /
    `llm.completion` attribute; then those native events.
    `gen_ai.system_instructions` leads the prompt as a system message. */
export function llmMessages(span) {
  const attrs = span.attributes || {};
  const eventsOf = (name, direction) => (span.events || [])
    .filter((e) => e.name === name)
    .map((e) => ({
      direction,
      role: (e.attributes || {}).role,
      parts: [normalizePart({ content: (e.attributes || {}).content })],
    }));
  const side = (direction) => {
    const prompt = direction === 'prompt';
    const sources = [
      () => parseMessagesAttr(attrs[prompt ? 'gen_ai.input.messages' : 'gen_ai.output.messages'], direction),
      () => indexedMessages(attrs, prompt ? 'gen_ai.prompt.' : 'gen_ai.completion.', '', direction),
      () => indexedMessages(attrs, prompt ? 'llm.input_messages.' : 'llm.output_messages.', 'message.', direction),
      () => valueMessage(attrs[prompt ? 'input.value' : 'output.value'], direction),
      () => valueMessage(attrs[prompt ? 'llm.prompt' : 'llm.completion'], direction),
      () => eventsOf(prompt ? 'llm.prompt' : 'llm.completion', direction),
    ];
    for (const source of sources) {
      const found = source();
      if (found.length) return found;
    }
    return [];
  };
  const prompt = side('prompt');
  const instructions = attrs['gen_ai.system_instructions'];
  if (instructions != null && instructions !== '') {
    const parsed = parseMaybeJson(instructions);
    prompt.unshift(Array.isArray(parsed)
      ? { direction: 'prompt', role: 'system', parts: parsed.map(normalizePart) }
      : { direction: 'prompt', role: 'system', parts: [normalizePart(isPayloadRef(parsed) ? parsed : { content: parsed })] });
  }
  return [...prompt, ...side('completion')];
}

/** The messages inside a loaded payload body, normalized like any other
    turn, or null when the text is not a messages array. This is the second
    half of the `offloadedMessages` contract: an oversized
    gen_ai.{input,output}.messages attribute round-trips through the payload
    store as its original JSON, so fetching and re-parsing it yields exactly
    what would have rendered had it stayed inline — media parts included. */
export function parseLoadedMessages(text, direction) {
  if (typeof text !== 'string' || !text.trim()) return null;
  let parsed;
  try { parsed = JSON.parse(text); } catch (e) { return null; }
  const list = Array.isArray(parsed) ? parsed : [parsed];
  if (!list.length || !list.every((m) => m != null && (typeof m === 'object' || typeof m === 'string'))) return null;
  return list.map((m) => normalizeMessage(m, direction));
}

/** Plain-text rendering of a message, for copy-to-clipboard. */
export function messageText(message) {
  return (message.parts || []).map((part) => {
    switch (part.kind) {
      case 'text': return part.text;
      case 'tool_call': return part.name + '(' + (typeof part.args === 'string' ? part.args : JSON.stringify(part.args)) + ')';
      case 'tool_result': return typeof part.result === 'string' ? part.result : JSON.stringify(part.result);
      case 'media': return '[' + part.mediaType + (part.filename ? ' ' + part.filename : '') + (part.uri ? ' ' + part.uri : '') + ']';
      case 'payload': return part.preview;
      default: return '';
    }
  }).filter(Boolean).join('\n');
}
