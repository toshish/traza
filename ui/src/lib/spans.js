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

// A single message's rendered text is capped: sub-threshold prompts can still
// be hundreds of KB, and pasting that into the DOM freezes the panel. The full
// value is always available under Attributes / Offloaded payloads.
const MAX_MESSAGE_CHARS = 4000;
// Longer tool arguments/results are summarized rather than dumped.
const MAX_PART_JSON = 400;

function clip(text, limit) {
  return text.length > limit ? text.slice(0, limit) + ' … (' + text.length + ' chars)' : text;
}

function bytesLabel(bytes) {
  if (typeof bytes !== 'number' || !Number.isFinite(bytes)) return null;
  if (bytes >= 1 << 20) return (bytes / (1 << 20)).toFixed(1) + ' MiB';
  if (bytes >= 1 << 10) return (bytes / (1 << 10)).toFixed(1) + ' KiB';
  return bytes + ' B';
}

// Renders one message part. Media parts (image/audio/video/document) are
// described, never inlined: a base64 `data:` blob is megabytes of noise that
// tells the reader nothing the descriptor does not.
function describePart(part) {
  if (part == null || typeof part !== 'object') return String(part ?? '');
  if (isPayloadRef(part.content)) {
    return '[offloaded ' + (bytesLabel(part.content.bytes) || '') + '] ' + (part.content.preview || '');
  }
  if (part.type === 'text' && typeof part.content === 'string') return part.content;
  if (part.type === 'tool_call') {
    return '[tool_call ' + (part.name || '') + ' ' + clip(JSON.stringify(part.arguments ?? {}), MAX_PART_JSON) + ']';
  }
  if (part.type === 'tool_call_response') {
    return '[tool_result ' + clip(JSON.stringify(part.result ?? part.response ?? ''), MAX_PART_JSON) + ']';
  }
  // Media and unknown part types: a compact descriptor.
  const bits = [part.type || 'part'];
  if (part.mime_type) bits.push(part.mime_type);
  if (part.filename) bits.push(part.filename);
  const size = bytesLabel(part.size_bytes);
  if (size) bits.push(size);
  if (typeof part.uri === 'string') bits.push(part.uri);
  else if (typeof part.data === 'string') bits.push('inline ' + part.data.length + ' chars');
  else if (!part.type) return clip(JSON.stringify(part), MAX_PART_JSON);
  return '[' + bits.join(' · ') + ']';
}

// Flattens one OTel GenAI message ({role, parts:[{type,content|...}]} or the
// older {role, content}) to { role, content } where content is a string (or a
// payload reference passed through untouched).
function flattenGenAiMessage(message, direction) {
  if (message == null || typeof message !== 'object') {
    return { direction, role: undefined, content: message };
  }
  const role = message.role;
  if (isPayloadRef(message.content)) return { direction, role, content: message.content };
  if (typeof message.content === 'string') {
    return { direction, role, content: clip(message.content, MAX_MESSAGE_CHARS) };
  }
  const parts = Array.isArray(message.parts) ? message.parts : null;
  if (!parts) return { direction, role, content: clip(JSON.stringify(message), MAX_MESSAGE_CHARS) };
  const text = parts.map(describePart).join('\n');
  return { direction, role, content: clip(text, MAX_MESSAGE_CHARS) };
}

// Parses a gen_ai.{input,output}.messages attribute, which OpenLLMetry emits
// JSON-encoded (a string) but native ingest may carry as an array. A whole
// attribute past the offload threshold arrives as a {$payload} reference —
// surface it (preview + byte count) instead of silently rendering nothing.
function parseMessagesAttr(value, direction) {
  if (value == null) return [];
  if (isPayloadRef(value)) return [{ direction, role: undefined, content: value }];
  let arr = value;
  if (typeof value === 'string') {
    try { arr = JSON.parse(value); } catch (e) { return [{ direction, role: undefined, content: clip(value, MAX_MESSAGE_CHARS) }]; }
  }
  if (!Array.isArray(arr)) return [];
  return arr.map((m) => flattenGenAiMessage(m, direction));
}

/** Chat turns recovered from an LLM span, in order. Recognizes, newest OTel
    convention first: JSON gen_ai.input.messages / gen_ai.output.messages
    ({role, parts:[{type,content}]}); then legacy indexed attributes
    gen_ai.prompt.{i}.{role,content} / gen_ai.completion.{i}.{role,content};
    then Traza's native llm.prompt / llm.completion events. Content may be an
    offloaded-payload reference (see isPayloadRef). */
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
      .map(([, m]) => ({ direction: kind, role: m.role, content: m.content }));
  };
  const events = (span.events || [])
    .filter((e) => e.name === 'llm.prompt' || e.name === 'llm.completion')
    .map((e) => ({
      direction: e.name === 'llm.prompt' ? 'prompt' : 'completion',
      role: (e.attributes || {}).role,
      content: (e.attributes || {}).content,
    }));
  return [...collectIndexed('prompt'), ...collectIndexed('completion'), ...events];
}
