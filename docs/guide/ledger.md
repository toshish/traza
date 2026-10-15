# The ledger and evidence packs

A generation's manifest proves what the store holds **now**: every
load-bearing file, with its digest. It cannot prove what the store held last
quarter, or that today's manifest descends from that one. Every checkpoint
writes a new manifest, and whoever controls the disk could rewrite the whole
set so it stays consistent.

The ledger supplies that history. It is one append-only file,
`ledger.jsonl` in the data directory, holding a hash chain. Each published
generation, each published erasure, and each external countersignature adds
one entry, and each entry's head commits to the head of the entry before it.
An **evidence pack** bundles the chain, its verification, the retention
policy, activity counts and erasures into one document with one digest, and
that digest is what you give an external notary to sign.

Nothing here needs Traza to check it. The formula below is the whole format,
and the [ten-line script](#recomputing-the-heads-yourself) recomputes every
head with the Python standard library.

## What it proves, and what it does not

Read this section before relying on the ledger for anything.

**It proves:**

- **The current state matches a committed manifest.** Verification
  re-digests every file the live generation's manifest names, and checks that
  the latest checkpoint entry names that manifest's digest.
- **The commit history was not rewritten after an anchor.** You need to have
  kept the anchored head (or the notary's signature over it) somewhere other
  than this store. Rewriting any earlier entry changes its head and breaks
  every later link. Recomputing the whole tail so the links agree again then
  changes the anchored head, and that no longer matches your copy.
- **A known erasure was recorded.** Anyone who knows an erasure's subject
  can recompute its `subject_digest` and find the entry, and the chain shows
  where it sits relative to the generations around it.

**It does not prove:**

- **Anything about history, unless you anchor.** The chain has no secret in
  it. Anyone who can write `ledger.jsonl` can rewrite it and recompute every
  head, and verification will report it intact. What makes a rewrite
  detectable is an anchor held outside the store.
- **The contents of generations compacted or expired away before an
  anchor.** The ledger stores manifest digests, not files. Once the files are
  gone, nobody can re-check what they held, the ledger included.
- **That files were untampered when they were committed.** Anyone with write
  access to the data directory can alter files between two checkpoints, and
  the next checkpoint digests whatever it finds. The ledger makes committed
  history tamper-evident; it does not make the live directory tamper-proof.
- **Anything about data that was never sent.** That covers spans a client
  never exported, spans refused at admission, and spans a pending erasure
  suppressed. The ledger records what the store committed, not what happened
  in the world.
- **Signatures.** Anchors are stored exactly as submitted. Traza checks only
  that `target_head` is the head recorded at `target_seq`. Whether the
  signature is valid is for you to check with the notary's tooling and public
  key.
- **When things happened.** Each `at_ns` is the server's wall clock and an
  anchor's `anchored_at_ns` is the notary's claim. Neither is a trusted
  timestamp.
- **That an erasure subject stays secret.** A `subject_digest` hides the
  subject from someone who does not know it. A guessable subject, such as a
  short or sequential session id, can still be confirmed by guessing.

## The chain

Every entry has these common fields:

| Field | Meaning |
|---|---|
| `seq` | Position in the chain, from 0 |
| `kind` | `genesis`, `checkpoint`, `erasure`, or `anchor` |
| `at_ns` | When the entry was appended (server wall clock, Unix ns) |
| `prev` | The previous entry's `head`; 64 zeros for the genesis entry |
| `head` | This entry's head (formula below) |

Each kind then adds its own fields:

| Kind | Fields | Appended when |
|---|---|---|
| `genesis` | `store_id`: 32 random hex characters | The ledger is first opened |
| `checkpoint` | `generation`, `manifest_sha256` | A published generation's manifest differs from the last one ledgered: every checkpoint, backup pin, erasure publication, restore install and format migration |
| `erasure` | `erasure_id`, `tenant` (`""` for the default tenant, `null` for a store-global payload subject), `subject_kind`, `subject_digest` | An erasure settles |
| `anchor` | `target_seq`, `target_head`, `authority`, `anchored_at_ns`, `signature`, `public_key`, `note` (optional) | [`POST /v1/ledger/anchors`](http-api.md#post-v1ledgeranchors) accepts one |

The head is:

```text
head = sha256("traza-ledger/1\n" + prev + "\n" + canonical_json(entry without "head"))
```

The hash is SHA-256 in lowercase hex. Canonical JSON means object keys
sorted, no whitespace, standard JSON string escaping (with non-ASCII left
as-is), and integers in decimal. Entries carry only strings, integers and
`null`, so any standard JSON library produces the same bytes.

The other two digests:

- `manifest_sha256` is the SHA-256 of `generations/<id>/state-manifest.json`,
  byte for byte. It is the same file a pin copies, so `sha256sum` reproduces
  it.
- `subject_digest` is the SHA-256 of the canonical JSON of the erasure
  subject, as [`GET /v1/erasures`](http-api.md#get-v1erasures) shows it. For
  example, `{"kind":"session","session_id":"…","tenant":"acme"}`. The
  `tenant` key is left out for the default tenant. The subject text itself is
  never written to the ledger.

**How fast it grows.** A checkpoint entry is about 305 bytes, measured. The
server checkpoints every five minutes whether or not anything changed, which
works out to 288 checkpoint entries, roughly 88 KB, per day. Erasures,
backups and explicit checkpoints add to that. Nothing rotates or compacts the
ledger.

**Crash consistency.** An entry is appended only once the fact it records is
durable: a checkpoint entry after `CURRENT` moves, an erasure entry after its
settle record. A crash in between therefore leaves a fact with no entry, never
an entry with no fact. The next open reconciles that. If the latest checkpoint
entry names a different manifest from the live one, it ledgers the live
generation, and it ledgers every settled erasure the chain does not yet carry.
The same reconcile ledgers generations published while no store was open,
such as a restore's install. If an append fails because of an I/O error, the
checkpoint or erasure reports that error, but what it recorded stays
published. A generation whose entry failed is then missing from the chain;
the next checkpoint or restart ledgers whichever generation is live by then.
An erasure whose entry failed is ledgered at the next open.

**Damage never stops the store.** At open, Traza truncates a torn final append
(an unterminated line that does not parse, which is what a crash mid-write
leaves), just as it does for the other append-only logs. Anything else is left
exactly as found: the store keeps appending after the last entry it can read,
and verification names every break.

## Recomputing the heads yourself

This script needs `python3` and nothing else. It reads `ledger.jsonl` (or a
pin's copy) and prints one verdict per entry:

```python
import hashlib, json, sys
prev, ok = "0" * 64, True
for n, line in enumerate(open(sys.argv[1], encoding="utf-8")):
    entry = json.loads(line); head = entry.pop("head")
    body = json.dumps(entry, sort_keys=True, separators=(",", ":"), ensure_ascii=False)
    want = hashlib.sha256(f"traza-ledger/1\n{prev}\n{body}".encode()).hexdigest()
    good = entry["seq"] == n and entry["prev"] == prev and head == want
    print(entry["seq"], entry["kind"], head, "ok" if good else "BROKEN")
    prev, ok = head, ok and good
sys.exit(0 if ok else 2)
```

```text
$ python3 check_ledger.py /var/lib/traza/ledger.jsonl
0 genesis 7af89af762ca8b14ec772a0f47e24136075f89163618b9cd9453a95005c541ed ok
1 checkpoint 829a2f44e1b9d65b1f8dadd5df1d6a01b2c7b3465771b17db61ac79decb1fd2b ok
2 checkpoint 0dee27bf2cb8dd9fb141908e863e72dfb6964515b564eeb27bb824eac34adf45 BROKEN
3 checkpoint eb5610460e9657ed3ec706d3831bbff1ee732b9cc6f3365f55519f769d752ac9 ok
```

In that run, one digit of entry 2 had been edited. The entry still parses and
its `prev` still links, but its head no longer matches its contents.

To check an anchor, confirm that its `target_head` is the head the script
printed for `target_seq`. Then verify `signature` over that head with the
notary's own tools.

## Verifying

`GET /v1/ledger/verify` and `traza-server verify --ledger` each perform
three checks:

1. Recompute every head and check every link. Entries are checked as generic
   JSON, so a field added by hand breaks the head, and every problem names its
   entry (`seq 2: head does not match its contents (…)`).
2. Check that the latest checkpoint entry names the live generation and its
   manifest digest.
3. Re-digest every file the live manifest names. This is the same check as
   [`GET /v1/verify`](http-api.md#get-v1verify).

```json
{"entries":5,"head":"13c3…","intact":true,"problems":[],
 "generation":{"id":3,"manifest_sha256":"97bc…","matches_ledger":true,
               "files_verified":true,"verified_at_ns":1792061580401844000}}
```

`intact` is true only when all three checks pass.

**What it costs.** The file check reads every manifested byte, so the cost
grows with store size. It runs under the maintenance lock, so compaction,
expiry and checkpoints wait for it to finish.

**After compaction or expiry.** Both replace segment files between
checkpoints. Until the next checkpoint publishes, the live manifest can name
files that are gone, and the check reports them as missing. For a clean
verification, run `POST /v1/checkpoint` first.

The offline form runs against a data directory that no live server owns:

```sh
traza-server verify --ledger --data-dir /var/lib/traza [--json]
```

It exits `0` when intact, `2` when anything is broken (every problem printed),
and `1` when it cannot verify at all: the directory is not a store, or a live
server owns it. It is read-only. It takes the directory lock and reads
`CURRENT`, the manifest, the files and the ledger exactly as they are. It runs
no recovery and appends nothing, because appending is the very thing under
examination.

## Anchoring

An anchor is an external notary's countersignature over a head, stored back
into the chain:

1. Read the tip with `GET /v1/ledger/head`, which returns
   `{"seq","head","store_id","at_ns"}`.
2. Have the notary sign `head`. Any scheme works: a timestamping authority, a
   signing service, a transparency log, or a key you control offline. Keep the
   notary's receipt **outside this store**.
3. Record the anchor with `POST /v1/ledger/anchors`. This needs the unbound
   `admin` scope.

```sh
curl -X POST http://localhost:8080/v1/ledger/anchors \
  -H "Authorization: Bearer $ADMIN" -d '{
    "target_seq": 4,
    "target_head": "13c3f1df2ba7c1fadf6d7d966a0d19d25be4a6c1c2e9d3ead88f9fade0f43524",
    "authority": "notary.example",
    "anchored_at_ns": 1792061622000000000,
    "signature": "MEUCIQ…",
    "public_key": "MCowBQYDK2VwAyEA…",
    "note": "Q3 close"}'
```

The route answers `201` with the appended entry. It answers `409` when
`target_head` is not the head recorded at `target_seq`, including when that
seq does not exist yet. An anchor can name any earlier entry, not just the
tip.

Anchor as often as you need to be able to detect a rewrite. Anything committed
after your most recent anchor is protected only by the chain, which, as stated
above, has no secret.

## Evidence packs

`GET /v1/evidence?since=&until=&tenant=` returns one JSON document:

| Field | Contents |
|---|---|
| `format` | `traza-evidence/1` |
| `generated_at_ns`, `engine_version`, `store_id` | When it was generated, by which build, and for which chain |
| `tenant` | The tenant the pack covers, or `null` for the whole store |
| `window` | `{"since_ns","until_ns"}`; the defaults are the epoch and now |
| `ledger.head` | The tip, as `GET /v1/ledger/head` reports it |
| `ledger.entries` | Entries whose `at_ns` falls in the window, up to the head and at most 10,000. `ledger.entries_truncated` says whether more exist; page through the rest with `GET /v1/ledger` |
| `ledger.verification` | The verify document above |
| `retention` | `{"ttl_seconds","source"}`. `source` is `tenant` (a `--tenant-ttl` override), `global` (`--ttl-seconds`) or `none`. `ttl_seconds` is `null` whenever data is kept forever |
| `usage` | `null` in this build |
| `activity` | Spans, distinct traces, distinct sessions and errors among spans that **started** in the window and are still stored, plus `by_day` (`{"day","spans","errors","cost_usd"}` per UTC day) |
| `erasures` | Erasures settled in the window: `{"id","subject_kind","published_at_ns","conclusive","result"}`, where `conclusive` and `result` are the [receipt's](http-api.md#get-v1erasuresidverify) own fields |
| `digest` | SHA-256 of the canonical JSON of every field above |

**Scoping.** A credential bound to a tenant always gets its own tenant's pack.
Naming a different tenant gets `403`. An unbound caller can name a tenant
(`tenant=` with an empty value means the default tenant) or leave it out to get
the whole store.

A tenant's pack contains only that tenant's activity and erasures. Other
tenants' erasure entries become stubs that keep `seq`, `kind`, `at_ns`, `prev`
and `head`, plus `"redacted": true`. The chain can still be followed across a
stub, but you cannot recompute the stub's own head or tell whose erasure it
was. File problems in a tenant pack's verification are reduced to a count,
because they would otherwise name store-wide files.

**Bounds.** Activity comes from the same fold the analytics use, so the query
deadline (`--query-deadline-ms`) applies. A window too large to fold within it
answers `400` with no partial pack; narrow the window. At most 32 erasure
receipts run per pack, and only while that same budget lasts. Past either
limit, `conclusive` and `result` are `null`, and
`GET /v1/erasures/{id}/verify` answers for that erasure. The pack's file
verification may reuse a verification of the same live generation from the
last 60 seconds, so that bound credentials cannot each force a full-store
re-read; `verified_at_ns` says when the file check ran.
`GET /v1/ledger/verify` never reuses a result.

Distinct traces and sessions are counted by a 128-bit hash of the tenant and
the id. An adversary who crafts colliding ids can make the count read low by
the number of collisions they manufacture.

**Recomputing the digest.** `cost_usd` values are JSON numbers with a
fractional part. Python would re-print them in a different format, so the
check keeps each one exactly as the server wrote it:

```python
import hashlib, json, sys
class Raw(str): pass  # a non-integer number, kept exactly as the server wrote it
def canon(v):
    if isinstance(v, dict):
        return "{" + ",".join(json.dumps(k, ensure_ascii=False) + ":" + canon(v[k]) for k in sorted(v)) + "}"
    if isinstance(v, list):
        return "[" + ",".join(canon(x) for x in v) + "]"
    return str(v) if isinstance(v, Raw) else json.dumps(v, ensure_ascii=False)
pack = json.load(open(sys.argv[1], encoding="utf-8"), parse_float=Raw)
claimed = pack.pop("digest")
ok = hashlib.sha256(canon(pack).encode()).hexdigest() == claimed
print("digest ok" if ok else "digest MISMATCH")
sys.exit(0 if ok else 2)
```

Have the notary sign `digest`. To tie the pack into the chain itself, anchor
`ledger.head` as well.

## Pins and restore

A backup pin (`POST /v1/backups/{label}`) copies the ledger into the pin
directory as `pins/<label>/ledger.jsonl`, next to `state-manifest.json`. The
last entry of that copy is the checkpoint for the pinned generation, and its
`manifest_sha256` is the SHA-256 of the pin's `state-manifest.json`. The
backup therefore carries its own evidence. Pin verification ignores the copy,
because it is not manifested.

A restore does not install a backup's ledger. Restoring into a directory that
already has a ledger continues that chain. Restoring into an empty directory
starts a new chain with a new `store_id`. Either way, the installed generation
is ledgered at the first open. Keep the backup's `ledger.jsonl` alongside it
if you need the source store's history.

An object-storage archive published from a pin does not carry the ledger
copy. It archives manifested files only.

## Limitations

- **No anchoring is built in.** Traza stores anchors and checks their
  `target_head`, but it never calls a notary.
- **`usage` is `null`.** The per-tenant usage counters are a separate feature.
- **Retention in a whole-store pack** reports the global window only.
  Per-tenant overrides appear in tenant packs.
- **No rotation.** The ledger grows by one line per published generation,
  forever. Every open reads it in full to find the tip and the erasures already
  ledgered.
