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

// Flattens one OTel GenAI message ({role, parts:[{type,content|...}]} or the
// older {role, content}) to { role, content } where content is a string (or a
// payload reference passed through untouched). Non-text parts (tool_call,
// tool_call_response) are rendered compactly so nothing is silently dropped.
function flattenGenAiMessage(message, direction) {
  if (message == null || typeof message !== 'object') {
    return { direction, role: undefined, content: message };
  }
  const role = message.role;
  if (isPayloadRef(message.content)) return { direction, role, content: message.content };
  if (typeof message.content === 'string') return { direction, role, content: message.content };
  const parts = Array.isArray(message.parts) ? message.parts : null;
  if (!parts) return { direction, role, content: JSON.stringify(message) };
  const text = parts.map((p) => {
    if (p == null || typeof p !== 'object') return String(p ?? '');
    if (p.type === 'text' && typeof p.content === 'string') return p.content;
    if (p.type === 'tool_call') return '[tool_call ' + (p.name || '') + ' ' + JSON.stringify(p.arguments ?? {}) + ']';
    if (p.type === 'tool_call_response') return '[tool_result ' + JSON.stringify(p.result ?? p.response ?? '') + ']';
    return JSON.stringify(p);
  }).join('\n');
  return { direction, role, content: text };
}

// Parses a gen_ai.{input,output}.messages attribute, which OpenLLMetry emits
// JSON-encoded (a string) but native ingest may carry as an array.
function parseMessagesAttr(value, direction) {
  let arr = value;
  if (typeof value === 'string') {
    try { arr = JSON.parse(value); } catch (e) { return [{ direction, role: undefined, content: value }]; }
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
