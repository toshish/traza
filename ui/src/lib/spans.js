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

/** LLM usage figures when the span carries llm.* attributes, else null. */
export function llmUsage(span) {
  const attrs = span.attributes || {};
  const model = attrs['llm.model'];
  const prompt = attrs['llm.prompt_tokens'];
  const completion = attrs['llm.completion_tokens'];
  const total = attrs['llm.total_tokens'];
  const cost = attrs['llm.cost_usd'];
  if (model == null && prompt == null && completion == null && total == null && cost == null) return null;
  return {
    model,
    promptTokens: typeof prompt === 'number' ? prompt : null,
    completionTokens: typeof completion === 'number' ? completion : null,
    totalTokens: typeof total === 'number' ? total
      : (typeof prompt === 'number' || typeof completion === 'number') ? (prompt || 0) + (completion || 0) : null,
    costUsd: typeof cost === 'number' ? cost : null,
    stopReason: attrs['llm.stop_reason'],
  };
}

export function sessionIdOf(span) {
  const id = (span.attributes || {})['session.id'];
  return typeof id === 'string' && id ? id : null;
}
