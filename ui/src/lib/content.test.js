import { describe, it, expect } from 'vitest';
import { detectFormat, prettyJson, parseMarkdown, parseInline } from './content.js';

describe('detectFormat', () => {
  it('recognizes JSON only when it actually parses', () => {
    expect(detectFormat('{"a": 1}')).toBe('json');
    expect(detectFormat('[1, 2]')).toBe('json');
    expect(detectFormat('{not really json}')).not.toBe('json');
  });

  it('recognizes markdown by its structural markers', () => {
    expect(detectFormat('## Heading')).toBe('markdown');
    expect(detectFormat('- one\n- two')).toBe('markdown');
    expect(detectFormat('```sql\nselect 1\n```')).toBe('markdown');
    expect(detectFormat('| a | b |\n|---|---|')).toBe('markdown');
    expect(detectFormat('a **bold** word')).toBe('markdown');
  });

  it('leaves prose as plain text', () => {
    expect(detectFormat('Revenue is up 12% and margin is flat.')).toBe('text');
    expect(detectFormat('')).toBe('text');
    expect(detectFormat(null)).toBe('text');
  });
});

describe('prettyJson', () => {
  it('formats valid JSON and refuses the rest', () => {
    expect(prettyJson('{"a":1}')).toBe('{\n  "a": 1\n}');
    expect(prettyJson('nope')).toBeNull();
  });
});

describe('parseMarkdown', () => {
  it('parses the block types the renderer supports', () => {
    const blocks = parseMarkdown([
      '# Title',
      '',
      'A paragraph.',
      '',
      '1. first',
      '2. second',
      '',
      '> quoted',
      '',
      '```sql',
      'select 1;',
      '```',
      '',
      '| Region | Revenue |',
      '|---|---|',
      '| NA | $4.2M |',
      '',
      '---',
    ].join('\n'));
    expect(blocks.map((b) => b.type)).toEqual([
      'heading', 'paragraph', 'list', 'quote', 'code', 'table', 'rule',
    ]);
    expect(blocks[0].level).toBe(1);
    expect(blocks[2].ordered).toBe(true);
    expect(blocks[4]).toMatchObject({ language: 'sql', code: 'select 1;' });
    expect(blocks[5].rows).toHaveLength(1);
  });

  it('keeps an unterminated code fence as code instead of losing it', () => {
    const blocks = parseMarkdown('```js\nconst a = 1;');
    expect(blocks).toHaveLength(1);
    expect(blocks[0]).toMatchObject({ type: 'code', code: 'const a = 1;' });
  });
});

describe('parseInline', () => {
  it('parses code, emphasis and links', () => {
    expect(parseInline('a `c` b **bold** c *it*')).toEqual([
      { kind: 'plain', text: 'a ' },
      { kind: 'code', text: 'c' },
      { kind: 'plain', text: ' b ' },
      { kind: 'bold', text: 'bold' },
      { kind: 'plain', text: ' c ' },
      { kind: 'italic', text: 'it' },
    ]);
  });

  it('links only http(s), so model output cannot inject a javascript: URL', () => {
    expect(parseInline('[ok](https://example.com)')).toEqual([
      { kind: 'link', text: 'ok', href: 'https://example.com' },
    ]);
    // The invariant is what matters: no link is produced, and the source text
    // survives verbatim (how it splits into plain runs is incidental).
    for (const hostile of [
      '[bad](javascript:alert(1))',
      '[bad](JavaScript:alert(1))',
      '[bad](data:text/html,<script>)',
      '[bad](vbscript:msgbox)',
    ]) {
      const spans = parseInline(hostile);
      expect(spans.some((span) => span.kind === 'link')).toBe(false);
      expect(spans.map((span) => span.text).join('')).toBe(hostile);
    }
  });
});
