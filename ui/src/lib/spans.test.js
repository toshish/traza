import { describe, it, expect } from 'vitest';
import {
  llmUsage, sessionIdOf, llmMessages, messageText,
  base64ToDataUri, parseLoadedMessages, toolResultParts,
} from './spans.js';

const span = (attributes, extra = {}) => ({ attributes, ...extra });

describe('llmUsage', () => {
  it('prefers current OTel names over the deprecated ones', () => {
    const usage = llmUsage(span({
      'gen_ai.provider.name': 'openai',
      'gen_ai.system': 'legacy',
      'gen_ai.response.model': 'resp',
      'gen_ai.request.model': 'req',
      'gen_ai.usage.input_tokens': 7,
      'gen_ai.usage.prompt_tokens': 999,
      'gen_ai.usage.output_tokens': 3,
      'gen_ai.usage.completion_tokens': 999,
    }));
    expect(usage.provider).toBe('openai');
    expect(usage.model).toBe('resp');
    expect(usage.promptTokens).toBe(7);
    expect(usage.completionTokens).toBe(3);
    expect(usage.totalTokens).toBe(10);
  });

  it('still reads the deprecated names when they are all that is present', () => {
    const usage = llmUsage(span({
      'gen_ai.system': 'anthropic',
      'gen_ai.usage.prompt_tokens': 40,
      'gen_ai.usage.completion_tokens': 25,
    }));
    expect(usage.provider).toBe('anthropic');
    expect(usage.totalTokens).toBe(65);
  });

  it('recognizes an LLM span from operation name alone', () => {
    expect(llmUsage(span({ 'gen_ai.operation.name': 'chat' }))).not.toBeNull();
    expect(llmUsage(span({ 'http.method': 'GET' }))).toBeNull();
  });
});

describe('sessionIdOf', () => {
  it('follows the documented key precedence', () => {
    expect(sessionIdOf(span({ 'session.id': 'a', 'gen_ai.conversation.id': 'b' })))
      .toEqual({ id: 'a', key: 'session.id' });
    expect(sessionIdOf(span({ 'gen_ai.conversation.id': 'b' })))
      .toEqual({ id: 'b', key: 'gen_ai.conversation.id' });
    expect(sessionIdOf(span({ 'traceloop.association.properties.chat_id': 'c' })))
      .toEqual({ id: 'c', key: 'traceloop.association.properties.chat_id' });
  });

  it('stringifies a numeric id, matching the server-side normalizer', () => {
    expect(sessionIdOf(span({ 'gen_ai.conversation.id': 4711 })))
      .toEqual({ id: '4711', key: 'gen_ai.conversation.id' });
  });

  it('returns null when no recognized key is present', () => {
    expect(sessionIdOf(span({ 'user.id': 'u' }))).toBeNull();
  });
});

describe('llmMessages', () => {
  it('parses the current JSON messages with role and parts', () => {
    const messages = llmMessages(span({
      'gen_ai.input.messages': JSON.stringify([
        { role: 'user', parts: [{ type: 'text', content: 'Hi' }] },
      ]),
      'gen_ai.output.messages': JSON.stringify([
        { role: 'assistant', parts: [{ type: 'text', content: 'Hello' }], finish_reason: 'stop' },
      ]),
    }));
    expect(messages).toHaveLength(2);
    expect(messages[0]).toMatchObject({ direction: 'prompt', role: 'user' });
    expect(messages[0].parts[0]).toEqual({ kind: 'text', text: 'Hi' });
    expect(messages[1].finishReason).toBe('stop');
  });

  it('classifies media, and only renders sources a browser can fetch', () => {
    const [message] = llmMessages(span({
      'gen_ai.output.messages': JSON.stringify([{
        role: 'assistant',
        parts: [
          { type: 'image', mime_type: 'image/png', filename: 'a.png', data: 'data:image/png;base64,AAA' },
          { type: 'video', mime_type: 'video/mp4', filename: 'v.mp4', uri: 's3://bucket/v.mp4' },
        ],
      }]),
    }));
    expect(message.parts[0]).toMatchObject({ kind: 'media', mediaType: 'image', src: 'data:image/png;base64,AAA' });
    // An object-store locator is a reference, not a broken image.
    expect(message.parts[1]).toMatchObject({ kind: 'media', mediaType: 'video', src: null, uri: 's3://bucket/v.mp4' });
  });

  it('parses tool calls and results', () => {
    const [message] = llmMessages(span({
      'gen_ai.output.messages': JSON.stringify([{
        role: 'assistant',
        parts: [{ type: 'tool_call', id: 'c1', name: 'lookup', arguments: '{"a":1}' }],
      }]),
    }));
    expect(message.parts[0]).toMatchObject({ kind: 'tool_call', name: 'lookup', id: 'c1' });
  });

  it('falls back to the legacy indexed attributes', () => {
    const messages = llmMessages(span({
      'gen_ai.prompt.0.role': 'user',
      'gen_ai.prompt.0.content': 'legacy in',
      'gen_ai.completion.0.role': 'assistant',
      'gen_ai.completion.0.content': 'legacy out',
    }));
    expect(messages.map((m) => m.parts[0].text)).toEqual(['legacy in', 'legacy out']);
  });

  it('falls back to native llm.prompt / llm.completion events', () => {
    const messages = llmMessages(span({}, {
      events: [
        { name: 'llm.prompt', attributes: { content: 'evt in' } },
        { name: 'llm.completion', attributes: { content: 'evt out' } },
      ],
    }));
    expect(messages.map((m) => m.parts[0].text)).toEqual(['evt in', 'evt out']);
  });

  it('surfaces an offloaded payload instead of rendering nothing', () => {
    const [message] = llmMessages(span({
      'gen_ai.input.messages': { $payload: 'sha256/abc', bytes: 900000, preview: 'start…' },
    }));
    expect(message.parts[0]).toMatchObject({ kind: 'payload', ref: 'sha256/abc', preview: 'start…' });
    // …and marks it as a whole offloaded conversation, so the renderer knows
    // the payload body parses back into messages.
    expect(message.offloadedMessages).toBe(true);
  });

  // The wild population of media part shapes. Each provider spells "here is
  // an image" differently; every spelling must land on a renderable source
  // or an honest reference, never on a JSON dump or a base64 wall.
  const B64 = 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAAB';

  it('lifts bare base64 bytes into a data: URI (OTel GenAI shape)', () => {
    const [message] = llmMessages(span({
      'gen_ai.input.messages': JSON.stringify([{
        role: 'user',
        parts: [{ type: 'image', mime_type: 'image/png', filename: 's.png', data: B64 }],
      }]),
    }));
    expect(message.parts[0]).toMatchObject({
      kind: 'media', mediaType: 'image', src: 'data:image/png;base64,' + B64, uri: undefined,
    });
  });

  it('reads the Anthropic source object, base64 and url alike', () => {
    const [message] = llmMessages(span({
      'gen_ai.input.messages': JSON.stringify([{
        role: 'user',
        content: [
          { type: 'image', source: { type: 'base64', media_type: 'image/png', data: B64 } },
          { type: 'image', source: { type: 'url', url: 'https://cdn.example/frame.jpg' } },
        ],
      }]),
    }));
    expect(message.parts[0]).toMatchObject({ kind: 'media', mediaType: 'image', mime: 'image/png', src: 'data:image/png;base64,' + B64 });
    expect(message.parts[1]).toMatchObject({ kind: 'media', mediaType: 'image', src: 'https://cdn.example/frame.jpg' });
  });

  it('recognizes typeless Google parts by their inline_data and file_data', () => {
    const [message] = llmMessages(span({
      'gen_ai.input.messages': JSON.stringify([{
        role: 'user',
        parts: [
          { inline_data: { mime_type: 'image/png', data: B64 } },
          { fileData: { mimeType: 'video/mp4', fileUri: 'gs://bucket/clip.mp4' } },
        ],
      }]),
    }));
    expect(message.parts[0]).toMatchObject({ kind: 'media', mediaType: 'image', src: 'data:image/png;base64,' + B64 });
    expect(message.parts[1]).toMatchObject({ kind: 'media', mediaType: 'video', src: null, uri: 'gs://bucket/clip.mp4' });
  });

  it('reads OpenAI input_audio, image_url objects, and file parts', () => {
    const [message] = llmMessages(span({
      'gen_ai.input.messages': JSON.stringify([{
        role: 'user',
        content: [
          { type: 'input_audio', input_audio: { data: B64, format: 'wav' } },
          { type: 'image_url', image_url: { url: 'data:image/png;base64,' + B64 } },
          { type: 'file', file: { filename: 'q3.pdf', file_data: 'data:application/pdf;base64,' + B64 } },
        ],
      }]),
    }));
    expect(message.parts[0]).toMatchObject({ kind: 'media', mediaType: 'audio', mime: 'audio/wav', src: 'data:audio/wav;base64,' + B64 });
    expect(message.parts[1]).toMatchObject({ kind: 'media', mediaType: 'image', src: 'data:image/png;base64,' + B64 });
    expect(message.parts[2]).toMatchObject({ kind: 'media', mediaType: 'document', filename: 'q3.pdf', src: 'data:application/pdf;base64,' + B64 });
  });

  it('says why bytes are missing when the emitter did not capture them', () => {
    const [message] = llmMessages(span({
      'gen_ai.input.messages': JSON.stringify([{
        role: 'user',
        parts: [{
          type: 'image', mime_type: 'image/jpeg', filename: 'probe.jpg', size_bytes: 38399,
          archive_status: 'unavailable', capture_status: 'unavailable',
          unavailable_reason: 'outside_allowed_roots',
        }],
      }]),
    }));
    expect(message.parts[0]).toMatchObject({
      kind: 'media', mediaType: 'image', src: null, uri: undefined,
      unavailable: true, unavailableReason: 'outside_allowed_roots',
    });
  });

  it('carries dimensions and keeps http archive locators renderable', () => {
    const [message] = llmMessages(span({
      'gen_ai.input.messages': JSON.stringify([{
        role: 'user',
        parts: [{
          type: 'image', mime_type: 'image/jpeg', filename: 'sheet.jpg',
          uri: 'http://127.0.0.1:8000/trace-media/ab/cd.jpg',
          archive_status: 'archived', width: 960, height: 540,
        }],
      }]),
    }));
    expect(message.parts[0]).toMatchObject({
      kind: 'media', src: 'http://127.0.0.1:8000/trace-media/ab/cd.jpg', width: 960, height: 540,
    });
  });

  it('never pours an opaque blob into the transcript as a reference', () => {
    // Not base64 (bad charset), not a URI (no scheme): the one honest answer
    // is the chrome with no body, NOT thousands of junk characters.
    const blob = ('&*^%$#@!'.repeat(4000));
    const [message] = llmMessages(span({
      'gen_ai.input.messages': JSON.stringify([{
        role: 'user',
        parts: [{ type: 'image', mime_type: 'image/png', data: blob }],
      }]),
    }));
    expect(message.parts[0]).toMatchObject({ kind: 'media', src: null, uri: undefined });
  });

  it('keeps a payload reference when the media bytes were offloaded', () => {
    // Native ingest can carry the attribute as a real array, not JSON text.
    const [message] = llmMessages(span({
      'gen_ai.input.messages': [{
        role: 'user',
        parts: [{ type: 'image', mime_type: 'image/png', data: { $payload: 'sha256/def', bytes: 1234 } }],
      }],
    }));
    expect(message.parts[0]).toMatchObject({
      kind: 'media', mediaType: 'image', src: null,
      payloadRef: { ref: 'sha256/def', bytes: 1234 },
    });
  });

  it('keeps unparseable content as text rather than dropping it', () => {
    const [message] = llmMessages(span({ 'gen_ai.input.messages': 'not json at all' }));
    expect(message.parts[0]).toEqual({ kind: 'text', text: 'not json at all' });
  });
});

describe('base64ToDataUri', () => {
  it('builds a data: URI from plausible base64 and a MIME type', () => {
    expect(base64ToDataUri('iVBORw0KGgoAAAANSUhEUg==', 'image/png'))
      .toBe('data:image/png;base64,iVBORw0KGgoAAAANSUhEUg==');
  });

  it('tolerates whitespace line-wrapping inside the bytes', () => {
    expect(base64ToDataUri('iVBORw0KGgo\nAAAANSUhEUg==', 'image/png'))
      .toBe('data:image/png;base64,iVBORw0KGgoAAAANSUhEUg==');
  });

  it('refuses words, short strings, and non-base64 blobs', () => {
    expect(base64ToDataUri('summary', 'audio/wav')).toBeNull();     // too short
    expect(base64ToDataUri('not base64 at all, clearly!!', 'x')).toBeNull();
    expect(base64ToDataUri('AAAA'.repeat(4) + 'A', 'x')).toBeNull(); // bad padding
  });

  it('falls back to octet-stream when the MIME is unknown', () => {
    expect(base64ToDataUri('AAAA'.repeat(5), undefined))
      .toBe('data:application/octet-stream;base64,' + 'AAAA'.repeat(5));
  });
});

describe('parseLoadedMessages', () => {
  it('parses a fetched messages payload into renderable turns', () => {
    const text = JSON.stringify([
      { role: 'assistant', parts: [
        { type: 'text', content: 'Here.' },
        { type: 'audio', mime_type: 'audio/wav', data: 'data:audio/wav;base64,AAAA' },
      ], finish_reason: 'stop' },
    ]);
    const messages = parseLoadedMessages(text, 'completion');
    expect(messages).toHaveLength(1);
    expect(messages[0]).toMatchObject({ direction: 'completion', role: 'assistant', finishReason: 'stop' });
    expect(messages[0].parts[1]).toMatchObject({ kind: 'media', mediaType: 'audio', src: 'data:audio/wav;base64,AAAA' });
  });

  it('returns null for non-JSON, scalars, and empty bodies', () => {
    expect(parseLoadedMessages('a plain prompt body', 'prompt')).toBeNull();
    expect(parseLoadedMessages('42', 'prompt')).toBeNull();
    expect(parseLoadedMessages('[]', 'prompt')).toBeNull();
    expect(parseLoadedMessages('', 'prompt')).toBeNull();
    expect(parseLoadedMessages('[1, 2]', 'prompt')).toBeNull();
  });
});

describe('toolResultParts', () => {
  it('unpacks an MCP-style content list that carries media', () => {
    const parts = toolResultParts({ content: [
      { type: 'text', text: 'Screenshot captured.' },
      { type: 'image', mimeType: 'image/png', data: 'iVBORw0KGgoAAAANSUhEUg==' },
    ] });
    expect(parts).toHaveLength(2);
    expect(parts[0]).toEqual({ kind: 'text', text: 'Screenshot captured.' });
    expect(parts[1]).toMatchObject({ kind: 'media', mediaType: 'image', src: 'data:image/png;base64,iVBORw0KGgoAAAANSUhEUg==' });
  });

  it('accepts a bare array as well as {content: […]}', () => {
    expect(toolResultParts([{ type: 'image', mimeType: 'image/png', data: 'iVBORw0KGgoAAAANSUhEUg==' }]))
      .toHaveLength(1);
  });

  it('stays null for plain values, so JSON results render as JSON', () => {
    expect(toolResultParts('done')).toBeNull();
    expect(toolResultParts({ ok: true })).toBeNull();
    expect(toolResultParts({ content: [{ type: 'text', text: 'no media here' }] })).toBeNull();
    expect(toolResultParts([])).toBeNull();
  });
});

describe('messageText', () => {
  it('renders every part kind for copy-to-clipboard', () => {
    const [message] = llmMessages(span({
      'gen_ai.output.messages': JSON.stringify([{
        role: 'assistant',
        parts: [
          { type: 'text', content: 'see this' },
          { type: 'image', mime_type: 'image/png', filename: 'a.png', uri: 's3://b/a.png' },
        ],
      }]),
    }));
    expect(messageText(message)).toBe('see this\n[image a.png s3://b/a.png]');
  });
});

// ---------------------------------------------------------------- self time
// Added with the rebuilt waterfall: a bar shows where time WAS, self time
// shows where it WENT, and the two differ whenever a span has children.

import { selfTimeNs, criticalPath, childrenOf } from './spans.js';

const node = (id, parent, start, end) => ({
  span_id: id, parent_span_id: parent, trace_id: 't',
  start_time_ns: start, end_time_ns: end, name: id, service: 's', status: 'ok',
});

describe('selfTimeNs', () => {
  it('is the whole duration when a span has no children', () => {
    expect(selfTimeNs(node('a', null, 0, 100), [])).toBe(100);
  });

  it('subtracts a child that covers part of the span', () => {
    expect(selfTimeNs(node('a', null, 0, 100), [node('b', 'a', 10, 60)])).toBe(50);
  });

  it('unions overlapping children rather than summing them', () => {
    // Two concurrent tool calls are the normal case in an agent trace.
    // Summing them would double-count the overlap and report negative
    // self time on a span that was simply waiting on both at once.
    const children = [node('b', 'a', 0, 60), node('c', 'a', 30, 80)];
    expect(selfTimeNs(node('a', null, 0, 100), children)).toBe(20);
  });

  it('never goes negative when children outlast their parent', () => {
    expect(selfTimeNs(node('a', null, 0, 50), [node('b', 'a', 0, 90)])).toBe(0);
  });

  it('counts a gap between children as the parent working', () => {
    const children = [node('b', 'a', 0, 20), node('c', 'a', 60, 80)];
    expect(selfTimeNs(node('a', null, 0, 100), children)).toBe(60);
  });
});

describe('criticalPath', () => {
  it('follows the child that finishes last at each level', () => {
    const spans = [
      node('root', null, 0, 100),
      node('slow', 'root', 10, 90),
      node('fast', 'root', 10, 20),
      node('deep', 'slow', 20, 85),
    ];
    const path = criticalPath(spans);
    expect(path.has('root')).toBe(true);
    expect(path.has('slow')).toBe(true);
    expect(path.has('deep')).toBe(true);
    expect(path.has('fast')).toBe(false);
  });

  it('is empty for an empty trace rather than throwing', () => {
    expect(criticalPath([]).size).toBe(0);
  });
});

describe('childrenOf', () => {
  it('groups spans under their parent and ignores roots', () => {
    const spans = [node('root', null, 0, 10), node('a', 'root', 1, 2), node('b', 'root', 3, 4)];
    const kids = childrenOf(spans);
    expect(kids.get('root').map((s) => s.span_id)).toEqual(['a', 'b']);
    expect(kids.has(null)).toBe(false);
  });
});
