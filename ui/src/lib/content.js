// Content-shape detection for model output. An assistant turn may be prose,
// markdown, a JSON object from a structured-output call, or fenced code; each
// wants a different presentation, and guessing wrong is worse than plain text.

/** 'json' | 'markdown' | 'text' — the shape to render `text` as. */
export function detectFormat(text) {
  if (typeof text !== 'string') return 'text';
  const trimmed = text.trim();
  if (!trimmed) return 'text';
  if ((trimmed.startsWith('{') && trimmed.endsWith('}')) || (trimmed.startsWith('[') && trimmed.endsWith(']'))) {
    if (parseJson(trimmed) !== undefined) return 'json';
  }
  if (looksMarkdown(trimmed)) return 'markdown';
  return 'text';
}

function parseJson(text) {
  try { return JSON.parse(text); } catch (e) { return undefined; }
}

/** Pretty-printed JSON, or null when `text` is not JSON. */
export function prettyJson(text) {
  const value = parseJson(String(text).trim());
  return value === undefined ? null : JSON.stringify(value, null, 2);
}

function looksMarkdown(text) {
  return (
    /^```/m.test(text) ||          // fenced code
    /^#{1,6}\s+\S/m.test(text) ||  // heading
    /^\s*[-*+]\s+\S/m.test(text) ||// bullet list
    /^\s*\d+\.\s+\S/m.test(text) ||// ordered list
    /^\s*>\s+\S/m.test(text) ||    // blockquote
    /^\s*\|.+\|\s*$/m.test(text) ||// table row
    /\*\*[^*\n]+\*\*/.test(text) ||// bold
    /\[[^\]\n]+\]\([^)\s]+\)/.test(text) // link
  );
}

/** Parses a markdown subset into block descriptors. Deliberately returns DATA,
    not HTML: the renderer builds React elements from it, so model output can
    never inject markup. */
export function parseMarkdown(text) {
  const lines = String(text).replace(/\r\n?/g, '\n').split('\n');
  const blocks = [];
  let i = 0;
  while (i < lines.length) {
    const line = lines[i];

    if (/^\s*```/.test(line)) {
      const language = line.replace(/^\s*```/, '').trim();
      const body = [];
      i += 1;
      while (i < lines.length && !/^\s*```/.test(lines[i])) { body.push(lines[i]); i += 1; }
      i += 1; // closing fence
      blocks.push({ type: 'code', language, code: body.join('\n') });
      continue;
    }

    const heading = line.match(/^(#{1,6})\s+(.*)$/);
    if (heading) {
      blocks.push({ type: 'heading', level: heading[1].length, spans: parseInline(heading[2]) });
      i += 1;
      continue;
    }

    if (/^\s*([-*_])\1{2,}\s*$/.test(line)) { blocks.push({ type: 'rule' }); i += 1; continue; }

    // A table needs a header row and a separator row of dashes.
    if (/^\s*\|.*\|\s*$/.test(line) && i + 1 < lines.length && /^\s*\|[\s:|-]+\|\s*$/.test(lines[i + 1])) {
      const cells = (row) => row.trim().replace(/^\||\|$/g, '').split('|').map((c) => parseInline(c.trim()));
      const header = cells(line);
      const rows = [];
      i += 2;
      while (i < lines.length && /^\s*\|.*\|\s*$/.test(lines[i])) { rows.push(cells(lines[i])); i += 1; }
      blocks.push({ type: 'table', header, rows });
      continue;
    }

    if (/^\s*>\s?/.test(line)) {
      const body = [];
      while (i < lines.length && /^\s*>\s?/.test(lines[i])) { body.push(lines[i].replace(/^\s*>\s?/, '')); i += 1; }
      blocks.push({ type: 'quote', spans: parseInline(body.join(' ')) });
      continue;
    }

    const bullet = /^\s*[-*+]\s+(.*)$/;
    const ordered = /^\s*\d+\.\s+(.*)$/;
    if (bullet.test(line) || ordered.test(line)) {
      const isOrdered = ordered.test(line);
      const pattern = isOrdered ? ordered : bullet;
      const items = [];
      while (i < lines.length && pattern.test(lines[i])) {
        items.push(parseInline(lines[i].match(pattern)[1]));
        i += 1;
      }
      blocks.push({ type: 'list', ordered: isOrdered, items });
      continue;
    }

    if (!line.trim()) { i += 1; continue; }

    const body = [];
    while (i < lines.length && lines[i].trim() && !/^\s*(```|#{1,6}\s|>\s|[-*+]\s|\d+\.\s|\|)/.test(lines[i])) {
      body.push(lines[i]);
      i += 1;
    }
    if (body.length) blocks.push({ type: 'paragraph', spans: parseInline(body.join(' ')) });
    else i += 1;
  }
  return blocks;
}

/** Inline spans: code, bold, italic, links, plain. */
export function parseInline(text) {
  const out = [];
  // Order matters: code first so ** inside backticks stays literal.
  const pattern = /(`[^`]+`)|(\*\*[^*]+\*\*)|(__[^_]+__)|(\*[^*\n]+\*)|(_[^_\n]+_)|(\[[^\]]+\]\([^)\s]+\))/;
  let rest = String(text);
  while (rest) {
    const match = rest.match(pattern);
    if (!match) { out.push({ kind: 'plain', text: rest }); break; }
    if (match.index > 0) out.push({ kind: 'plain', text: rest.slice(0, match.index) });
    const token = match[0];
    if (token.startsWith('`')) out.push({ kind: 'code', text: token.slice(1, -1) });
    else if (token.startsWith('**') || token.startsWith('__')) out.push({ kind: 'bold', text: token.slice(2, -2) });
    else if (token.startsWith('[')) {
      const link = token.match(/^\[([^\]]+)\]\(([^)\s]+)\)$/);
      // Only http(s) links are followable; anything else renders as text so a
      // javascript: URL in model output cannot become a live link.
      if (link && /^https?:\/\//i.test(link[2])) out.push({ kind: 'link', text: link[1], href: link[2] });
      else out.push({ kind: 'plain', text: token });
    } else out.push({ kind: 'italic', text: token.slice(1, -1) });
    rest = rest.slice(match.index + token.length);
  }
  return out;
}
