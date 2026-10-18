/**
 * Lossless JSON for Traza's responses, and a writer that can send a
 * nanosecond timestamp back.
 *
 * `JSON.parse` turns every number into a double, and Traza's timestamps are
 * Unix nanoseconds — around 1.8e18, far past `Number.MAX_SAFE_INTEGER`
 * (about 9.0e15). `JSON.parse` would round them to the nearest multiple of
 * 256 without a word, and a ledger head recomputed from a rounded `at_ns`
 * would not match. So responses are read here instead:
 *
 * - every integer under a key ending in `_ns` is a `bigint`, whatever its
 *   size, so a timestamp field has one type;
 * - any other integer too large for a double to hold exactly is a `bigint`;
 * - everything else is what `JSON.parse` would give.
 *
 * Recorded payloads — a tape step's `request` and `response`, a tool step's
 * `arguments` and `result` — are the application's own data, and are read
 * exactly as `JSON.parse` reads them, so they look the way the application
 * that recorded them would see them.
 *
 * @module
 */

/** A non-integer JSON number, kept as the exact text the server wrote. */
export class RawNumber {
  constructor(readonly text: string) {}
  toString(): string {
    return this.text;
  }
}

/**
 * `JSON.stringify` that writes a `bigint` as a JSON integer instead of
 * throwing, so a nanosecond timestamp read from Traza can be sent back
 * exactly. Otherwise it follows `JSON.stringify`: `toJSON` is honoured,
 * `undefined` and functions are left out of objects (and are `null` in
 * arrays, as is a sparse array's hole), and a non-finite number is `null`.
 */
export function stringifyJson(value: unknown): string {
  const text = write(value, false);
  if (text === undefined) throw new TypeError("value has no JSON representation");
  return text;
}

function write(value: unknown, inArray: boolean): string | undefined {
  if (value !== null && typeof value === "object" && typeof (value as { toJSON?: unknown }).toJSON === "function") {
    value = (value as { toJSON: () => unknown }).toJSON();
  }
  switch (typeof value) {
    case "bigint":
      return value.toString();
    case "string":
    case "number":
    case "boolean":
      return JSON.stringify(value);
    case "undefined":
    case "function":
    case "symbol":
      return inArray ? "null" : undefined;
  }
  if (value === null) return "null";
  if (Array.isArray(value)) {
    // Every index, as JSON.stringify visits them: map() skips a sparse
    // array's holes, writing [1, , 3] as "[1,,3]" (not JSON) and
    // new Array(1) as "[]". A hole reads as undefined, which is null here.
    // The length is read once, as JSON.stringify reads it: an element's
    // toJSON may push onto this array, and following the live length would
    // never finish.
    const items: string[] = [];
    const length = value.length;
    for (let i = 0; i < length; i++) items.push(write(value[i], true) ?? "null");
    return "[" + items.join(",") + "]";
  }
  // The keys are taken once and each member read as it is written, as
  // JSON.stringify does: a toJSON hook's change to a later member is what
  // goes out, and a member it deletes is left out.
  const object = value as Record<string, unknown>;
  const parts: string[] = [];
  for (const key of Object.keys(object)) {
    const text = write(object[key], false);
    if (text !== undefined) parts.push(JSON.stringify(key) + ":" + text);
  }
  return "{" + parts.join(",") + "}";
}

/** Keys whose values are recorded application data, read verbatim. */
const PAYLOAD_KEYS = new Set(["request", "response", "arguments", "result"]);

const NUMBER = /-?(?:0|[1-9]\d*)(\.\d+)?([eE][+-]?\d+)?/y;

/**
 * Parses `text`. With `rawNumbers`, every integer is a `bigint` and every
 * other number a {@link RawNumber} — the exact form the evidence digest is
 * computed over.
 */
export function parseJson(text: string, options: { rawNumbers?: boolean } = {}): unknown {
  const parser = new Parser(text, options.rawNumbers === true);
  const value = parser.value(undefined, false);
  parser.whitespace();
  if (parser.at !== text.length) parser.fail("unexpected trailing characters");
  return value;
}

class Parser {
  at = 0;

  constructor(
    private readonly text: string,
    private readonly raw: boolean,
  ) {}

  fail(message: string): never {
    throw new SyntaxError(`invalid JSON at offset ${this.at}: ${message}`);
  }

  whitespace(): void {
    const text = this.text;
    while (this.at < text.length) {
      const c = text.charCodeAt(this.at);
      if (c !== 0x20 && c !== 0x0a && c !== 0x0d && c !== 0x09) return;
      this.at++;
    }
  }

  value(key: string | undefined, verbatim: boolean): unknown {
    this.whitespace();
    const c = this.text[this.at];
    switch (c) {
      case "{":
        return this.object(verbatim);
      case "[":
        return this.array(key, verbatim);
      case '"':
        return this.string();
      case "t":
        return this.literal("true", true);
      case "f":
        return this.literal("false", false);
      case "n":
        return this.literal("null", null);
      default:
        return this.number(key, verbatim);
    }
  }

  object(verbatim: boolean): Record<string, unknown> {
    this.at++;
    const out: Record<string, unknown> = {};
    this.whitespace();
    if (this.text[this.at] === "}") {
      this.at++;
      return out;
    }
    for (;;) {
      this.whitespace();
      if (this.text[this.at] !== '"') this.fail("expected a key");
      const key = this.string();
      this.whitespace();
      if (this.text[this.at] !== ":") this.fail("expected ':'");
      this.at++;
      const value = this.value(key, verbatim || PAYLOAD_KEYS.has(key));
      // defineProperty, not assignment: assigning "__proto__" would replace
      // the object's prototype instead of adding a key, as JSON.parse never
      // does.
      Object.defineProperty(out, key, { value, enumerable: true, writable: true, configurable: true });
      this.whitespace();
      const next = this.text[this.at++];
      if (next === ",") continue;
      if (next === "}") return out;
      this.at--;
      this.fail("expected ',' or '}'");
    }
  }

  array(key: string | undefined, verbatim: boolean): unknown[] {
    this.at++;
    const out: unknown[] = [];
    this.whitespace();
    if (this.text[this.at] === "]") {
      this.at++;
      return out;
    }
    for (;;) {
      out.push(this.value(key, verbatim));
      this.whitespace();
      const next = this.text[this.at++];
      if (next === ",") continue;
      if (next === "]") return out;
      this.at--;
      this.fail("expected ',' or ']'");
    }
  }

  string(): string {
    const text = this.text;
    const start = this.at;
    let i = start + 1;
    while (i < text.length) {
      const c = text.charCodeAt(i);
      if (c === 0x5c) {
        i += 2;
        continue;
      }
      if (c === 0x22) break;
      i++;
    }
    if (i >= text.length) this.fail("unterminated string");
    this.at = i + 1;
    // The escapes are JSON's, so JSON.parse decodes (and validates) them.
    return JSON.parse(text.slice(start, i + 1)) as string;
  }

  literal<T>(word: string, value: T): T {
    if (!this.text.startsWith(word, this.at)) this.fail(`expected ${word}`);
    this.at += word.length;
    return value;
  }

  number(key: string | undefined, verbatim: boolean): number | bigint | RawNumber {
    NUMBER.lastIndex = this.at;
    const match = NUMBER.exec(this.text);
    if (!match) this.fail("expected a value");
    const token = match[0];
    this.at += token.length;
    const integer = match[1] === undefined && match[2] === undefined;
    if (this.raw) return integer ? BigInt(token) : new RawNumber(token);
    if (integer && !verbatim) {
      if ((key !== undefined && key.endsWith("_ns")) || !Number.isSafeInteger(Number(token))) {
        return BigInt(token);
      }
    }
    return Number(token);
  }
}
