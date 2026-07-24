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
// truth on the server). Traza recognizes the OpenLLMetry / OTel GenAI
// conventions (gen_ai.*, llm.usage.*, traceloop.*) and its own native llm.* /
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
    null. Resolves both the OpenLLMetry / OTel GenAI conventions and Traza's
    native llm.* shorthand (mirror of src/semconv.rs) — current OTel names
    first, the deprecated names OTel replaced as aliases. */
export function llmUsage(span) {
  const attrs = span.attributes || {};
  const model = firstStr(attrs, ['gen_ai.response.model', 'gen_ai.request.model', 'llm.model']);
  const provider = firstStr(attrs, ['gen_ai.provider.name', 'gen_ai.system']);
  const promptTokens = firstNum(attrs, ['gen_ai.usage.input_tokens', 'gen_ai.usage.prompt_tokens', 'llm.prompt_tokens']);
  const completionTokens = firstNum(attrs, ['gen_ai.usage.output_tokens', 'gen_ai.usage.completion_tokens', 'llm.completion_tokens']);
  const explicitTotal = firstNum(attrs, ['llm.usage.total_tokens', 'gen_ai.usage.total_tokens', 'llm.total_tokens']);
  const costUsd = firstNum(attrs, ['llm.cost_usd', 'gen_ai.usage.cost']);
  const stopReason = firstStr(attrs, ['gen_ai.response.finish_reason', 'gen_ai.response.stop_reason', 'llm.stop_reason']);
  const operation = firstStr(attrs, ['gen_ai.operation.name', 'llm.request.type']);
  const spanKind = firstStr(attrs, ['traceloop.span.kind']);
  const anything = model != null || provider != null || promptTokens != null || completionTokens != null
    || explicitTotal != null || costUsd != null || stopReason != null || operation != null || spanKind === 'llm';
  if (!anything) return null;
  const totalTokens = explicitTotal != null ? explicitTotal
    : (promptTokens != null || completionTokens != null) ? (promptTokens || 0) + (completionTokens || 0) : null;
  return { model, provider, promptTokens, completionTokens, totalTokens, costUsd, stopReason };
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
    if (t === 'input_audio') return 'audio';
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

// One part of a message, normalized into a shape the renderer can switch on.
function normalizePart(part) {
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
  const mime = part.mime_type || part.mimeType || part.media_type;
  const media = mediaKindOf(type, mime);
  if (media) {
    const raw = part.data ?? part.uri ?? part.url ?? part.image_url;
    const locator = typeof raw === 'object' && raw !== null ? raw.url : raw;
    return {
      kind: 'media',
      mediaType: media,
      mime: mime || undefined,
      filename: part.filename || part.name || undefined,
      sizeBytes: typeof part.size_bytes === 'number' ? part.size_bytes : undefined,
      src: renderableSrc(locator),
      uri: typeof locator === 'string' ? locator : undefined,
    };
  }
  if (typeof part.content === 'string') return { kind: 'text', text: clip(part.content, MAX_TEXT_CHARS) };
  if (typeof part.text === 'string') return { kind: 'text', text: clip(part.text, MAX_TEXT_CHARS) };
  return { kind: 'text', text: clip(JSON.stringify(part, null, 2), MAX_TEXT_CHARS) };
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
// surface it instead of silently rendering nothing.
function parseMessagesAttr(value, direction) {
  if (value == null) return [];
  if (isPayloadRef(value)) {
    return [{ direction, role: undefined, parts: [normalizePart(value)] }];
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

/** Chat turns recovered from an LLM span, in order, as structured messages:
    { direction: 'prompt'|'completion', role, finishReason, parts: [...] }.
    Part kinds are 'text', 'media', 'tool_call', 'tool_result', 'payload'.

    Recognizes, newest OTel convention first: JSON gen_ai.input.messages /
    gen_ai.output.messages ({role, parts:[{type,content}]}); then the legacy
    indexed gen_ai.prompt.{i}.{role,content} / gen_ai.completion.{i}.*; then
    Traza's native llm.prompt / llm.completion events. */
export function llmMessages(span) {
  const attrs = span.attributes || {};
  if (attrs['gen_ai.input.messages'] != null || attrs['gen_ai.output.messages'] != null) {
    return [
      ...parseMessagesAttr(attrs['gen_ai.input.messages'], 'prompt'),
      ...parseMessagesAttr(attrs['gen_ai.output.messages'], 'completion'),
    ];
  }
  const collectIndexed = (kind) => {
    const prefix = 'gen_ai.' + kind + '.';
    const byIndex = new Map();
    for (const [key, value] of Object.entries(attrs)) {
      if (!key.startsWith(prefix)) continue;
      const rest = key.slice(prefix.length); // e.g. "0.content" or "0.role"
      const dot = rest.indexOf('.');
      if (dot < 0) continue;
      const i = rest.slice(0, dot);
      const field = rest.slice(dot + 1);
      if (field !== 'role' && field !== 'content') continue;
      if (!byIndex.has(i)) byIndex.set(i, {});
      byIndex.get(i)[field] = value;
    }
    return [...byIndex.entries()]
      .sort((a, b) => Number(a[0]) - Number(b[0]))
      .map(([, m]) => ({ direction: kind, role: m.role, parts: [normalizePart({ content: m.content })] }));
  };
  const events = (span.events || [])
    .filter((e) => e.name === 'llm.prompt' || e.name === 'llm.completion')
    .map((e) => ({
      direction: e.name === 'llm.prompt' ? 'prompt' : 'completion',
      role: (e.attributes || {}).role,
      parts: [normalizePart({ content: (e.attributes || {}).content })],
    }));
  return [...collectIndexed('prompt'), ...collectIndexed('completion'), ...events];
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
