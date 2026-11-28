/**
 * The evidence pack's digest, recomputed.
 *
 * The digest is the SHA-256 of the canonical JSON — sorted keys, no
 * whitespace — of every field but `digest`. Numbers must be re-emitted
 * exactly as the server printed them: a double re-printed by JavaScript
 * (`1e-5` versus `0.00001`, or a nanosecond timestamp rounded) would change
 * the bytes and the digest with them. So the pack is re-read from its text
 * with every number kept verbatim. This is the check the ledger guide gives
 * in Python, in TypeScript.
 *
 * @module
 */

import { parseJson, RawNumber } from "./json.js";

/** Recomputes an evidence pack's `digest` from the pack's JSON text. */
export async function verifyEvidenceDigest(text: string): Promise<boolean> {
  const pack = parseJson(text, { rawNumbers: true });
  if (!pack || typeof pack !== "object" || Array.isArray(pack)) return false;
  const fields = pack as Record<string, unknown>;
  const claimed = fields.digest;
  if (typeof claimed !== "string") return false;
  delete fields.digest;
  return (await sha256Hex(canonical(fields))) === claimed;
}

function canonical(value: unknown): string {
  if (value === null) return "null";
  if (typeof value === "boolean") return value ? "true" : "false";
  if (typeof value === "bigint") return value.toString();
  if (value instanceof RawNumber) return value.text;
  if (typeof value === "string") return JSON.stringify(value);
  if (Array.isArray(value)) return "[" + value.map(canonical).join(",") + "]";
  if (typeof value === "object") {
    const record = value as Record<string, unknown>;
    const keys = Object.keys(record).sort(byCodePoint);
    return "{" + keys.map((key) => JSON.stringify(key) + ":" + canonical(record[key])).join(",") + "}";
  }
  throw new TypeError(`unexpected ${typeof value} in an evidence pack`);
}

/**
 * Orders keys by Unicode code point, as the server and Python's `sorted` do.
 * JavaScript's default sort compares UTF-16 code units, which puts a
 * character above U+FFFF before one in U+E000..U+FFFF.
 */
function byCodePoint(a: string, b: string): number {
  const left = Array.from(a);
  const right = Array.from(b);
  const length = Math.min(left.length, right.length);
  for (let i = 0; i < length; i++) {
    const difference = left[i]!.codePointAt(0)! - right[i]!.codePointAt(0)!;
    if (difference !== 0) return difference;
  }
  return left.length - right.length;
}

async function sha256Hex(text: string): Promise<string> {
  const subtle = globalThis.crypto?.subtle;
  if (!subtle) {
    // Browsers expose SubtleCrypto only in secure contexts (https, localhost).
    throw new Error("verifying a digest needs crypto.subtle (Node 20+, or a browser page served over https)");
  }
  const digest = await subtle.digest("SHA-256", new TextEncoder().encode(text));
  return Array.from(new Uint8Array(digest), (byte) => byte.toString(16).padStart(2, "0")).join("");
}
