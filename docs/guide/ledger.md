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
- **A recorded erasure still holds.** Verification checks every ledgered
  erasure against the live tombstone log. Each entry needs a settled erasure
  of its subject published by the entry's own `generation` or a later one,
  and verification names any entry that has none (see the restore bullet
  below).
- **Every settled erasure was recorded.** Verification also checks the other
  way: every settled erasure in the live tombstone log must have its entry,
  matched by `erasure_id`, `subject_digest` and `generation`.

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
  short or sequential session id, can still be confirmed by guessing by
  anyone who sees the full entry (unbound credentials). For a tenant subject
  the entry's `tenant` field is the tenant's name.
- **That an erasure was never undone.** Restoring a pin taken before an
  erasure brings the subject's data back, together with the tombstone log as
  it was. The chain records the install as an ordinary `checkpoint` entry.
  Verification catches it only by comparing the ledgered erasure against the
  live tombstone log; it names the entry as no longer holding until the
  erasure is re-applied. Each entry is judged on its own. If a subject was
  erased twice and the restored pin falls between the two erasures, the
  first erasure is still settled, but it does not count for the second. The
  second erasure's entry is named, because the data it removed is back. An
  erasure re-applied after such a restore gets a new entry. Its `erasure_id`
  can repeat an earlier entry's (the tombstone log was rewound), but its
  `generation` will not.
- **That other tenants learn nothing.** A tenant's evidence pack shows when
  other tenants' erasures were ledgered: each one is a stub with its `seq` and
  `at_ns`. `GET /v1/ledger/head`, open to bound credentials, also shows when
  the chain grows. Neither shows whose erasure it was or what it covered.

## See it run

[`examples/prove/run.sh`](../../examples/prove/README.md) walks through this
guide against a real server. It ingests and checkpoints, reads the head and
the chain, verifies it with `GET /v1/ledger/verify`, and recomputes every head
from `ledger.jsonl` with the [script below](#recomputing-the-heads-yourself).
It records an anchor signed by a stand-in notary, erases a session and shows
the erasure entry carrying a `subject_digest` that it recomputes — the
session id appears nowhere in the file — and recomputes an evidence pack's
digest with the [standard-library check](#evidence-packs). Then it stops the
server, flips one byte in a sealed segment, and `traza-server verify
--ledger` exits `2` naming the file. It takes about two seconds and exits
non-zero if any claim fails.

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
| `erasure` | `erasure_id`, `tenant` (`""` for the default tenant, `null` for a store-global payload subject), `subject_kind`, `subject_digest`, `generation` (the generation that published the erasure), `salt` (32 random hex characters) | An erasure settles |
| `anchor` | `target_seq`, `target_head`, `authority`, `anchored_at_ns`, `signature`, `public_key`, `note` (optional) | [`POST /v1/ledger/anchors`](http-api.md#post-v1ledgeranchors) accepts one |

The head is:

```text
head = sha256("traza-ledger/1\n" + prev + "\n" + canonical_json(entry without "head"))
```

The hash is SHA-256 in lowercase hex. Canonical JSON means object keys
sorted, no whitespace, standard JSON string escaping (with non-ASCII left
as-is), and integers in decimal. Entries carry only strings, integers and
`null`, so any JSON library that parses integers exactly produces the same
bytes. Python's `json` module does. JavaScript's `JSON.parse` does not: every
`at_ns` and `anchored_at_ns` is around 1.8 × 10^18, above 2^53, and parsing
them as doubles rounds them, so the recomputed heads and pack digests will
not match. In JavaScript, use a parser that keeps big integers exact (as
`BigInt` or as their source text); any other doubles-only parser has the same
problem.

The `salt` in an erasure entry is there for tenant isolation. A tenant's
evidence pack shows other tenants' erasure entries as stubs without it, and
without the salt the stub's head can't be recomputed. Every other field that
goes into that head can be guessed: the erasure id is a small integer, the
subject kind is one of five, and a tenant subject's digest depends only on
the tenant's name. Without the salt, a tenant could try candidate neighbours
until a head matched.

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
such as a restore's install.

**A failed append never fails what it records.** If an append fails, for
example on a full disk or a `ledger.jsonl` the server may not write, the
checkpoint, erasure, backup pin or open it belongs to still succeeds. The
generation is already published, and the erasure is settled with its
admission barrier lifted. Verification reports the failure (`ledger
append(s) failed since …`) until the ledger catches up. The next checkpoint
ledgers the generation that is live by then, along with every settled
erasure the chain lacks. A generation published while appends were failing
may never get an entry of its own; only the live one is ledgered.

**Damage never stops the store.** At open, Traza truncates a torn final append
(an unterminated line that does not parse, which is what a crash mid-write
leaves), just as it does for the other append-only logs. Anything else is left
exactly as found: the store keeps appending after the last entry it can read,
and verification names every break. The one thing that stops an open is a
`ledger.jsonl` the server cannot **read**. Without the tip, an append would
start a second chain rather than extend the first.

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
these checks:

1. Recompute every head and check every link. Entries are checked as generic
   JSON, so a field added by hand breaks the head, and every problem names its
   entry (`seq 2: head does not match its contents (…)`).
2. Check that the latest checkpoint entry names the live generation and its
   manifest digest.
3. Re-digest every file the live manifest names. This is the same check as
   [`GET /v1/verify`](http-api.md#get-v1verify).
4. Check that every erasure entry still holds: the live tombstone log must
   have a settled erasure of the same subject, published by the entry's
   `generation` or a later one. Restoring a pin taken before an erasure fails
   this check, and the problem names the entry until the erasure is applied
   again.
5. Check that every settled erasure in the live tombstone log has its entry,
   with the same `erasure_id`, `subject_digest` and `generation`. A crash
   between an erasure's settle record and its entry leaves one missing
   (`erasure 3 (trace subject, settled in generation 7) is not in the
   ledger…`). So does a failed append. The next open ledgers it, and so does
   the next checkpoint after a failed append. A live check does not count an
   entry that is being appended while it runs as missing.
6. Report any ledger append that failed and has not been caught up
   (`GET /v1/ledger/verify` only, since a fresh offline process has no record
   of a running server's failures).

```json
{"entries":5,"head":"34e6…","intact":true,"problems":[],
 "problems_truncated":false,"chain_verified_at_ns":1792061580401790000,
 "generation":{"id":3,"manifest_sha256":"97bc…","matches_ledger":true,
               "files_verified":true,"verified_at_ns":1792061580401844000}}
```

`intact` is true only when every check passes. `chain_verified_at_ns` is
when the walk of the chain began at its first entry, and `verified_at_ns` is
when the file check ran. Both are the moment of the request here. In an
evidence pack they can be earlier (see [Bounds](#evidence-packs)). This
check names every problem, so `problems_truncated` is always `false` here;
an evidence pack names at most 100 of each kind.

**A live generation newer than the chain.** Suppose the latest checkpoint
entry names an older generation than the live one. That is exactly what
appears if the server crashed, or an append failed, between publishing a
generation and ledgering it, or if a restore installed a generation that no
store has opened since. The problem says so. The next open (or, on a live
server, the next checkpoint) ledgers the live generation and clears it. Any
other cause is a discrepancy to investigate. It still counts against
`intact`, because until that entry exists nothing commits the chain to the
live state.

**What it costs.** The file check reads every manifested byte, so the cost
grows with store size. It runs under the maintenance lock, so compaction,
expiry and checkpoints wait for it to finish. The chain walk does not hold
that lock. The chain is read only up to the tip as it stood when the check
began, and nothing rewrites those bytes, so the walk runs after the lock is
released. Erasure and anchor appends never wait. An entry appended mid-check
is left for the next check rather than read half-written.
`GET /v1/ledger/verify` always walks the whole chain and re-runs the file
check.

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
`CURRENT`, the manifest, the files, the tombstone log and the ledger exactly
as they are. It runs no recovery and appends nothing, because appending is the
very thing under examination. One consequence: on a store that crashed between
publishing a generation and ledgering it, it exits `2` with the
newer-than-the-chain problem above. A crash between settling an erasure and
ledgering it likewise exits `2`, with the not-in-the-ledger problem. Start the
server (or open the store) once, and both are ledgered. A tombstone-log line
that does not parse is skipped and counted as a problem, because the erasure
it records could not be checked. The store itself refuses to open over such a
line.

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
    "target_head": "34e64ba565ed353f7db1ab6f1e9611b8815053c45548c22b9827d2c3dfc415e5",
    "authority": "notary.example",
    "anchored_at_ns": 1792061622000000000,
    "signature": "MEUCIQ…",
    "public_key": "MCowBQYDK2VwAyEA…",
    "note": "Q3 close"}'
```

The route answers `201` with the appended entry. It answers `409` when
`target_head` is not the head recorded at `target_seq`. That includes a
`target_head` that is not 64 lowercase hex characters, and a seq that does
not exist yet. An anchor can name any earlier entry, not just the tip.

**Anchors are visible to every tenant.** An anchor entry appears whole in
every tenant's evidence pack, `authority` and `note` included, because it is
what lets a tenant's auditor check the chain with the notary. Keep anything
tenant-specific (a customer's name in a note, for instance) out of anchor
fields.

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
| `ledger.entries_unreadable` | How many of the chain's lines the pack could not place: not a JSON object, or no integer `seq` or `at_ns`. The verification names each one |
| `ledger.verification` | The verify document above, naming at most 100 problems of each kind (see below) |
| `retention` | `{"ttl_seconds","source"}`: the window expiry applies to the tenant right now. `source` is `limits` (the tenant's runtime [limits document](http-api.md#post-v1admintenantstenantlimits)), `flag` (a `--tenant-ttl` override), `global` (`--ttl-seconds`) or `none`. `ttl_seconds` is `null` whenever data is kept forever. A whole-store pack reports the global window |
| `usage` | The tenant's row from [`GET /v1/usage`](http-api.md#get-v1usage) — lifetime and month counters, limits, and quota state — as of generation. `null` in a whole-store pack |
| `activity` | Spans, distinct traces, distinct sessions and errors among spans that **started** in the window and are still stored, plus `by_day` (`{"day","spans","errors","cost_usd"}` per UTC day) |
| `erasures` | Erasures settled in the window, oldest first and at most 10,000: `{"id","subject_kind","published_at_ns","conclusive","result"}`, where `conclusive` and `result` are the [receipt's](http-api.md#get-v1erasuresidverify) own fields |
| `erasures_truncated` | Whether more erasures were settled in the window than `erasures` lists. [`GET /v1/erasures`](http-api.md#get-v1erasures) lists them all |
| `digest` | SHA-256 of the canonical JSON of every field above |

**Scoping.** A credential bound to a tenant always gets its own tenant's pack.
Naming a different tenant gets `403`. An unbound caller can name a tenant
(`tenant=` with an empty value means the default tenant) or leave it out to get
the whole store.

`ledger.head`, `ledger.entries` and `ledger.verification` come from one
snapshot of the chain. No entry in the pack is newer than the head it
reports, and the verification covers exactly that chain.

A tenant's pack contains only that tenant's activity and erasures. Other
tenants' erasure entries (and store-global payload erasures) become stubs
that keep `seq`, `kind`, `at_ns`, `prev` and `head`, plus `"redacted": true`.
The chain can still be followed across a stub. You cannot recompute the
stub's own head, because its `salt` is withheld, and you cannot tell whose
erasure it was. The stub does show when it was ledgered (see
[what it does not prove](#what-it-proves-and-what-it-does-not)). The tenant's
own erasure entries are whole, salt included, so they recompute. Anchor
entries are whole in every pack ([Anchoring](#anchoring)).

The verification in a tenant's pack keeps the chain's problems and the
tenant's own. File problems are reduced to a count, because they would
otherwise name store-wide files. Problems about other tenants' erasures are
withheld behind a count that does not say whose. `intact` is the store's
verdict either way: a pack never reports a store intact when it isn't.

A pack names at most 100 problems of each kind it shows: damage to the chain,
ledgered erasures the tombstone log no longer holds, settled erasures missing
from the chain, and failing files. The rest of a kind is one line giving how
many more there are and the total (`… are not named here (N in all)`), and
`problems_truncated` is `true`. A restore that undoes thousands of erasures
therefore costs each pack the problems it names, not one line per erasure,
and costs a tenant those erasures do not concern only the count. Problems
withheld from a tenant are counted, not truncated, so they leave
`problems_truncated` alone. `GET /v1/ledger/verify` and the offline check
name every problem.

**Bounds.** Bound credentials can request packs, so a pack must not cost the
whole chain, every erasure or the whole store on every request, and must not
hold off checkpoints, compaction or erasures while it works. The server keeps
what packs have checked, and each pack pays for what changed since the last.
One query budget (`--query-deadline-ms`) covers the whole request. Every step
below shares it, and none gets a fresh budget of its own.

- **Settled erasures** are read from the tombstone log once each, in the
  order they settled, and indexed. A pack takes in only the ones settled since
  the last pack.
- **The chain walk** continues the last completed walk (a pack's, or
  `GET /v1/ledger/verify`'s), so a pack normally walks only the entries
  appended since. It runs without the lock that checkpoints, compaction and
  erasures take. The erasure checks (steps 4 and 5 above) are kept with the
  walk and take in only the entries and settled erasures new to it.
- **A new generation's manifest** is read without that lock, a budget at a
  time. Only opening it takes the lock. A manifest is never rewritten once
  published, so the open file is a fixed snapshot of that generation's
  manifest even if a checkpoint deletes it meanwhile. Reading it, digesting
  it and decoding one entry per file stop at the budget and resume where they
  stopped. Until a pack is answered, packs stay with the snapshot of the chain
  and the generation that manifest belongs to, however many checkpoints land
  meanwhile, so a store that checkpoints between every retry still gets its
  pack. That pack's `ledger.head` is the head as it stood then, and every
  entry and finding in it is as of that head. The next pack takes the store
  as it is by then. Packs stop waiting on that snapshot once its manifest is
  in and it is more than 60 seconds old, or as soon as the generation is
  found to be no longer live while a file problem is being confirmed.
- **The file check** digests files without that lock too, a budget at a time.
  Each file is digested from one open handle, so a file the budget stopped
  part-way through is continued, not started again. A new generation
  re-digests only the files its checkpoint added or changed. The others keep
  their last digest, and when it ran. The append-only logs (annotations,
  evals, tombstones) grow at every checkpoint, so a pack keeps each one open
  with its digest so far and continues over the longer prefix the next
  manifest names, rather than starting again at byte 0. A pack continues
  only while the path still names the same file, so a log rewritten by
  expiry or an erasure is digested afresh. If a continued digest disagrees
  with the manifest, the log is digested again from byte 0 before anything
  is reported, because the earlier bytes may have been edited in place.
- **A file problem found without the lock is checked again under it** before
  a pack reports it. Compaction, expiry and checkpoints carry on while a pack
  digests, so a file the pack reaches after expiry removed or replaced it
  looks missing or changed although nothing is damaged. The problem stands
  only if the generation the pack is checking is still live and the path
  still holds the file the pack read, or still nothing. That is what
  `GET /v1/ledger/verify` would find at that moment, which includes
  reporting as missing a file compaction removed before the next checkpoint
  (see [Verifying](#verifying)). If the path holds a different file now, the
  pack digests that file. If a checkpoint has published a new generation,
  the pack checks that generation instead: it takes in the new manifest and
  digests what changed. A pack that keeps finding the store moved this way,
  eight times running, answers `409`; what it checked is kept, so a retry
  continues. Problems are checked again 256 at a time, each batch under its
  own hold of the lock, with the budget checked between batches, so a store
  with thousands of damaged files does not hold off checkpoints while a pack
  looks at all of them. Problems not yet checked again wait for the next
  request.
- **A file check the next generation replaces** is freed a budget's share at
  a time, at the start of later packs and in the time they spend on redoing
  old checks, and never while holding the lock.
- **The window's entries and erasures** are found by binary search in indexes
  by time, so a pack reads only those lines.
- **Activity** comes from the same fold the analytics use. A window too large
  to fold within the budget answers `400` with no partial pack; narrow the
  window.
- **Receipts:** at most 32 run per pack, and only while the budget lasts. Past
  either limit, `conclusive` and `result` are `null`, and
  `GET /v1/erasures/{id}/verify` answers for that erasure.

If the budget runs out in any of the first five steps, that step stops,
keeps its progress, and the pack is refused with `400` saying how far it got
(for example `the ledger chain was verified through line N …`, `N entries of
generation G's manifest … were taken in …`, `N of M files of generation G
were digested …`, or `N file problem(s) found off the maintenance lock were
still to be confirmed …`). A pack never carries a partial
verification. The next request resumes where the step stopped, however long
ago that was, so retrying completes the check even when it needs many
budgets. The first pack after a server starts does the most of this work,
because nothing has been checked yet.

**How old a pack's checks can be.** Once a completed chain walk or a file's
digest is more than 60 seconds old, packs start redoing it: a new walk from
the first entry, and a fresh digest of each file, oldest first. Each pack
spends what its budget has left after the pack is assembled, at most 250 ms,
on that redo. Meanwhile packs still answer from the older result and say how
old it is. `ledger.verification.chain_verified_at_ns` is when the walk the
verification rests on began at the first entry. `ledger.verification.generation.verified_at_ns`
is when the oldest file digest it rests on began. A redone walk is used once
it is complete. So an edit made to `ledger.jsonl` or to a manifested file
under a running server shows in packs once the redo reaches it. How long that
takes depends on the chain's and the store's size and on how often packs are
requested. `GET /v1/ledger/verify` never reuses a result and sees such an edit
at once.

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
`manifest_sha256` is the SHA-256 of the pin's `state-manifest.json`. An
erasure or anchor appended while the pin was being built is left out of the
copy. The backup therefore carries its own evidence. Pin verification ignores
the copy, because it is not manifested. If the ledger could not record the
pinned generation (a failed append, which verification reports), the pin is
still taken and carries no `ledger.jsonl`. A copy that ended anywhere else
would misstate what it evidences.

A restore does not install a backup's ledger. Restoring into a directory that
already has a ledger continues that chain. Restoring into an empty directory
starts a new chain with a new `store_id`. Either way, the installed generation
is ledgered at the first open. Keep the backup's `ledger.jsonl` alongside it
if you need the source store's history.

**Restoring an earlier pin can undo erasures.** A pin taken before an
erasure holds the subject's data and the tombstone log from before the
erasure. Restore it into the same directory and both come back, while the
chain keeps the erasure's entry. Verification names that entry until you
apply the erasure again. The re-application is ledgered as a new entry with
the same subject, often the same `erasure_id`, and a later `generation`. The
same holds when the pin falls between two erasures of one subject: only the
later one is undone, and verification names its entry even though the
earlier erasure is still settled.

An object-storage archive published from a pin does not carry the ledger
copy. It archives manifested files only.

## Limitations

- **No anchoring is built in.** Traza stores anchors and checks their
  `target_head`, but it never calls a notary.
- **A whole-store pack has no usage row,** and its retention is the global
  window only. Per-tenant usage and retention overrides appear in tenant packs.
- **Usage counts are not ledgered.** The pack's `usage` row is the in-memory
  counter at generation time, which a crash can undercount by a few seconds'
  worth (see [`GET /v1/usage`](http-api.md#get-v1usage)). The digest commits
  to what the pack said, not to the counts being complete.
- **No rotation.** The ledger grows by one line per published generation,
  forever. Every open reads it in full to find the tip and the erasures already
  ledgered. Once a server has answered an evidence pack or a verification, it
  also keeps about 48 bytes per entry in memory: the walk the next pack
  continues from (see [Bounds](#evidence-packs)). While packs redo a walk
  older than 60 seconds, a second walk is kept beside it. Packs also keep, in
  memory, a few hundred bytes per settled erasure (the index and the erasure
  checks) and one record per manifested file (the file check). While packs
  take in a new generation's manifest, they also hold its bytes and a second
  record per file. They keep up to one open file per append-only log, so a
  log replaced by expiry or an erasure keeps its disk space until the next
  pack checks that log.
- **Anchor signatures are stored, never checked.** See
  [what it does not prove](#what-it-proves-and-what-it-does-not).
- **A failed append is caught up, not retried immediately.** The next
  checkpoint (every five minutes on the server) or the next open ledgers what
  it missed. Until then, verification reports the failure.
- **Verification right after compaction or expiry** reports files as missing
  until the next checkpoint. See [Verifying](#verifying).
