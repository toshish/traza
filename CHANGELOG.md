# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Attribution: the server names the failing step, instead of the reader
  doing it.** A new `diagnose_session` MCP tool answers *why did this run
  fail* — the outcome, the step the failure is attributed to, and the
  repetition behind it — with the evidence for each claim travelling beside
  it. It replaces the two prompts that used to teach a model to read a trace
  and judge its shape by eye (*"repeated sibling names are a retry storm,
  deep chains are a loop"*), on the surface's own rule that a prompt wanting
  a branch is a tool.
  - **Every rule fires on telemetry that already exists.** An earlier design
    rested on a `session.outcome` attribute and a `relation: "retry-of"` link
    — both conventions Traza invented and nothing else emits — which would
    have been a closed loop: seed the convention, detect it, pass CI, and
    answer `cause: null` on every real store. A declared convention now
    raises confidence and is never a precondition. The discriminators are
    error density, serial fraction, self-similar depth and context growth.
  - **Context growth is read from `context_tokens`, not prompt tokens.**
    `gen_ai.usage.cache_read_input_tokens` and `cache_creation_input_tokens`
    are recognized, because Anthropic reports `input_tokens` as the *uncached
    remainder*: a growing conversation reports a prompt count that falls as
    its cache warms, so reading the wrong field inverts the signal on the
    configuration long-running agents actually use. Where the arithmetic is
    unknown the trend is reported as unreadable rather than guessed.
  - **It refuses to say more than the data supports.** A shape is classified
    only when a discriminator agrees; otherwise it is reported as
    inconclusive with the missing signal named. Ordinary iteration is
    reported *as* ordinary so a reader can see it was examined and set aside.
    Absent token data can never satisfy a healthy classification. The
    strongest test is silence: the analysis finds no fault anywhere in the
    bundled corpus of healthy workloads.
  - **Session outcome** is first-class and provenance-tagged: `declared` when
    a span said so (`session.outcome`, `session.goal` — Traza extensions,
    labelled as honestly as `llm.cost_usd`), `derived` from the run's own
    spans otherwise, and `unknown` — never rendered as success — for a run
    still in flight. A run that failed a call and recovered is a success that
    reports its errors, not a failure.
- **`promote_failures_to_dataset`** closes the loop into the M4 eval
  entities: a session's attributed failures become a regression dataset
  version, each example carrying its own copy plus provenance back to the
  span. Re-promoting is idempotent. Gated by an `rw` token **and** a new
  `--mcp-promote` switch, separate from `--mcp-annotations` because an
  annotation dies with its span while a promoted example deliberately
  outlives its source. **The caller names a session, never a span**: the
  promoted set is re-derived server-side from the diagnosis, so text injected
  into the telemetry can change whether a promotion happens but never what it
  copies — proven by a test that plants exactly that instruction.
- A **runaway agent scenario** in the seed corpus: an agent whose search tool
  fails from the fourth turn on, so every later reflection appends the failure
  to its context and asks again. Its steps are siblings under one root, the
  shape real frameworks emit, and it carries no declared outcome and no retry
  link — so finding it proves the analysis works on what a real pipeline
  sends.

### Fixed

- **Link attributes were outside every payload walker.** A `$payload`
  reference copied into a span's `links[].attributes` — an ordinary thing for
  an SDK to do, since the wire contract stores what a client sends — was
  counted by no reference sweep, redacted by no erasure, and invisible to the
  verify predicate. So a blob a surviving span still referenced could be
  deleted, and `verify --erasure` could report **erased and conclusive over
  content still on disk and still readable**. All five walkers now share one
  iterator naming every attribute map a span carries; the duplicate collector
  that made the gap exist twice is gone. Links are also bounded at ingest for
  the first time, with its own test. The two erasure guards are
  mutation-proven: with the link arm removed from the iterator, one test finds
  the whole secret sitting inline in the link and the other finds the payload
  deleted while a live reference remained.
- `top_failures` and `slowest_spans` advertised a row limit whose maximum
  (100) and default (20) were both wrong — they apply 50 and 10 — so a caller
  obeying the schema was silently given a fraction of what it asked for.
- The claim in the data-model and LLM-semantics guides that a link's
  `relation` attribute "keeps link semantics queryable" was false — no query
  path reaches link attributes. The docs now say what is true: links are
  traversed inside a diagnosis, and are not filterable.

### Added

- **Tenant identity in the primary key.** Span identity is now
  `(tenant, trace_id, span_id)` — everywhere: the write buffer's index, WAL
  replay, segment supersede resolution, compaction's last-write-wins merge,
  the tail ring's veils, cursors, the analytics key hashes, annotations and
  erasure. Two tenants sharing a trace id can no longer silently upsert over
  each other, which is the whole reason this ships now: keys cannot be
  retrofitted after the format freeze. The default tenant is the empty
  string and is **never serialized**, so a single-tenant store writes
  byte-identical WAL frames, segment records and annotation lines to what it
  wrote before tenancy existed — no segment or WAL format bump, proven by a
  serialization-guard test. Tenant scoping reaches every surface the roadmap
  names:
  - **Credentials**: `TRAZA_TOKENS` entries take an optional binding,
    `scope@tenant:token`. A bound credential ingests, queries, tails,
    exports, annotates and erases exactly its own tenant; naming another
    tenant is a 403, and the store-global operator surfaces (`/v1/stats`,
    `/v1/metrics`, `/v1/verify`, checkpoint/flush/backups) refuse bound
    tokens outright. OTLP exporters select a tenant with the `traza.tenant`
    resource attribute; MCP tools are scoped by the same binding.
  - **Sessions** are `(tenant, session_id)` — the same `session.id` under
    two tenants is two sessions, in the rollup sidecar (format v3,
    self-healing rebuild), the session list, and `group_by=session` rows.
  - **Retention** takes per-tenant windows: `--tenant-ttl TENANT=SECONDS`,
    repeatable. A tenant's cutoff is its override, else the global TTL,
    else never — and the segment retire-whole fast path only runs when a
    global TTL covers everyone, because a whole-segment decision taken from
    configured windows alone would delete an unswept tenant's data with the
    segment.
  - **Quota accounting**: `GET /v1/tenants` reports per-tenant spans,
    traces, serialized bytes and offloaded payload bytes from one exact
    fold. Accounting, deliberately not enforcement.
  - **Erasure**: trace/span/session subjects carry a tenant (empty = the
    default tenant, never "all"), and a new `tenant` subject erases
    everything a tenant owns — spans, annotations, scores, datasets,
    experiments — with the same barrier, ordering, and receipt discipline
    as M3. Reference-aware payload deletion now spans tenants: a blob two
    tenants share survives one tenant's erasure and the receipt names why.
- **The eval entity model — identity only, no workflow.** The addressing the
  product thesis requires, and nothing else: **Dataset** (stable id, name,
  tenant), **DatasetVersion** (immutable, content-addressed manifest of
  `(example_id, digest)` pairs with a parent version for lineage and the
  promotion's provenance — re-POSTing identical content IS the same
  version), **Example** (stable id across versions; input, optional
  expected output, split label, provenance back to the source span; bodies
  carry `$payload` references that count as live for the TTL sweep and
  reference-aware erasure, so a promoted copy is real for offloaded
  content), **Experiment** (stable id, one dataset version, config
  metadata), **Run** (the experiment→trace link, appended by the external
  harness), and **Score** — an annotation whose addressing was generalized
  to a typed subject (trace / span / session / experiment example), so a
  score addresses the `(experiment, example, span)` tuple with every
  existing annotation field preserved. All of it lives in `evals.jsonl`, a
  new manifested append-only recovery domain with the annotation log's
  torn-tail healing and pin-by-copy discipline. Deletion semantics were
  settled up front and are enforced by test: erasing source traces never
  corrupts a dataset version (the receipt's new `eval-records` domain
  REPORTS surviving copies and turns inconclusive — purging a curated copy
  is a deliberate second act); erasing a payload leaves dangling addresses
  in example bodies, reported retained-by-design without losing
  conclusiveness; a dataset-version tombstone is logical deletion with
  defined effects (410 with the tombstone, dependent experiments keep
  working and say why, new experiments refused); a tenant erasure takes the
  tenant's eval records inside the barrier, and ids are never reused past
  the rewrite (a counter record floors the allocators). Score distributions
  (`/summary`) and experiment-over-experiment diffs (`/diff`) come from
  ordinary reads with per-`(example, name)` last-write-wins dedup. The
  whole loop — promote failing traces, run externally, record runs and
  scores, read distributions and diffs — runs end to end in CI against the
  built binary, and survives kill -9.

- **One recovery domain: generations and checkpoints.** Query-visible state
  lived in several independent recovery domains — the write-ahead log and
  buffer, segments, `annotations.jsonl`, `payloads/` — each with its own
  durability rule and its own idea of "now", and nothing named a state they
  all agreed on. That is why backup, export, retention and deletion were four
  mechanisms rather than one. A **generation** is that agreed state: a
  manifest naming every load-bearing file with its SHA-256 digest, plus the
  log position it folded through. `CURRENT` names the live one, and moving it
  — a staged rename made durable by a directory fsync — is the single commit
  point for a checkpoint, a restore, or a published deletion.

  - **Backup runs against a live server**: `POST /v1/backups/{label}`
    checkpoints, hard-links the manifested files into `pins/{label}`, and
    verifies every digest before reporting success. Hard links share inodes,
    so a pin costs almost no disk and holds its bytes even after compaction
    unlinks the originals — the copy proceeds at its own pace, and
    `POST /v1/backups/{label}/release` frees it. The pinned set carries
    **spans, annotations and payload bytes together**, which is what a span
    export never could.
  - **Restore is install**: `traza-server --restore DIR` (or `Store::restore`)
    verifies the backup *before* swapping anything, and commits at one
    `CURRENT` rename, so a failed or interrupted restore leaves the prior
    store rather than a blend.
  - **`GET /v1/verify`** re-digests the live generation and names each
    discrepancy, so recovery can distinguish damage it may ignore from damage
    that changes what the store contains by asking rather than inferring it
    from whether parsing happened to succeed.
  - **`POST /v1/checkpoint`**, plus a checkpoint every five minutes from the
    maintenance thread. Cheap by construction: segments are immutable, so a
    checkpoint carries their digests over from the previous manifest and
    hashes only what was written since.

- **Log frames carry the generation they belong to.** The framing gains an
  eight-byte magic and a `(epoch, sequence)` stamp — a 24-byte header, up from
  8. `CURRENT` and the log are separate filesystem objects, so no rename can
  make "the new generation is live" and "the frames folded into it are gone"
  one event; a crash between them meets a log still holding folded frames, and
  for a checkpoint that published a deletion, replaying them resurrects
  exactly what was deleted. Recovery now replays a frame only when its stamp
  is strictly after the live generation's `folded_through`, which demotes log
  reclamation from correctness to housekeeping. Sequences are monotonic for
  the life of the store — a rewrite keeps counting rather than restarting —
  because a stamp landing at or before a recorded fold point would be
  discarded by the next replay as though it had been folded.

  `tests/generations.rs` proves it against the crash state itself: the log's
  pre-checkpoint bytes are written back over a committed generation, and the
  test fails when the stamp rule is removed.

- **Existing data directories are adopted at first open.** Nothing moves —
  segments stay exactly where they are, because their paths are load-bearing —
  and generation one is published over the files already there, converting a
  pre-generation log to stamped framing on the way. One-way, resumable, and
  committed by the `CURRENT` rename, so a crash mid-adoption leaves a
  directory the next open finishes.

- **Targeted deletion, with a receipt.** `POST /v1/erasures` erases a
  **subject** — a trace, a span, a session (resolved across every recognized
  session key), or one offloaded payload — and the new
  `traza-server verify --erasure` (or `GET /v1/erasures/{id}/verify`) then
  proves it: every domain the subject's bytes could inhabit, checked by name,
  with the result of each. TTL removes by age; this removes because someone
  is entitled to have it gone, and the receipt is the difference.

  - **The tombstone log** (`tombstones.jsonl`) is a new manifested,
    append-only recovery domain beside the annotation log. The intent record
    is fsynced *before* anything is removed, so from that moment the subject
    is invisible to every read path — search, lookups, sessions, analytics,
    annotations, payload fetches, export, the live tail — even before the
    rewrites run, and a crash mid-purge leaves a pending erasure the next
    open masks and the maintenance tick finishes. Idempotent to resume: the
    purge re-verifies rather than re-damages. (The settle record's counts
    are therefore the settling pass's counts; the receipt, not the tallies,
    is the authority on absence.)
  - **A pending erasure is an admission barrier, not just a veil.** Covered
    spans are dropped at ingest — acknowledged, never stored, counted in
    `traza_erasure_spans_suppressed_total` — and the barrier is total: it
    runs BEFORE payload offloading, so a suppressed span leaves no orphan
    payload bytes behind (a review probe planted exactly that orphan — the
    file of a span that never entered the store, invisible to record and
    receipt alike); an oversized value whose content hash is itself under
    erasure offloads directly to its redacted marker instead of recreating
    the file being deleted; and annotations addressed to a covered subject
    are dropped at admission the same way. The barrier holds at the BEGIN
    transition, not just in the steady pending state: admissions hold an
    **erasure gate** in read mode from their mask load through their store
    mutation — the offload's file writes included — and `begin`/settle move
    the mask only under its write mode, so a batch or annotation is wholly
    before an erasure or wholly after it, never astride it. A one-shot mask
    check could not say that, and review was right to test the transition.
    The cut is exact: every span acknowledged before `settled_unix_ns` is
    erased or was never stored, under concurrent writers hammering the
    subject (a review probe demonstrated 92 pre-settle survivors before the
    barrier existed; `tests/erasure.rs` keeps the probes as regression
    tests, plus a transition stress test that erases in a loop under
    payload-writing and annotating threads and then audits the payload
    directory itself for orphans). The ingest
    response tells the truth about it — `accepted` counts what was stored,
    `suppressed` appears when nonzero, and the OTLP surfaces answer with
    `partialSuccess.rejectedSpans` (hand-encoded for the protobuf path,
    because an empty response is a claim of full success).
  - **Nothing rewrites a manifested file after the checkpoint the settle
    cites.** The confirm purge and annotation drops moved BEFORE the
    checkpoint; the settle append itself rides the append-only allowance
    every manifest grants the tombstone log. A review probe caught the old
    order making the settle's own generation fail verification
    (`annotations.jsonl: digest mismatch`) the moment a concurrent
    annotation landed; the ordering is now barrier → purge → confirm →
    checkpoint → settle → lift, and the suite asserts the cited generation
    verifies clean, including under concurrent writers.
  - **The purge reaches every domain.** Buffer and write-ahead log rewritten
    to the survivors (the TTL discipline: a deletion a restart undoes is not
    a deletion); segments rewritten in place, superseded versions of an
    erased key included — the purge tests every physical record, not the
    visible one; annotations addressed to erased spans dropped; payload
    files deleted **reference-aware**, because content addressing means one
    file can back spans outside the subject — those bytes are retained and
    the receipt names the reason. A payload subject additionally rewrites
    every referencing span to drop its inline preview: the preview is
    content too. Publication is a checkpoint — the deletion is durable when
    `CURRENT` moves, and `tests/erasure.rs` proves it by killing the server
    after its 200.
  - **The receipt is a verification, not a claim.** Its result is computed
    from the walk, never from the settle record. Matches are classified
    against the erase record's resolved keys under one rule for every
    domain: an erased key found live again is a re-delivery and fails the
    receipt; a fresh key under the same identifiers is new activity,
    reported without failing it — an erasure is a barrier, not a ban. Pins
    are checked and named: a backup pinned before the erasure still holds
    the bytes in its hard-link farm, and the receipt says which pin to
    release. What remains afterwards is stated rather than hidden: the
    tombstone record itself keeps the subject's identifiers and content
    hashes — never the erased text — as the record the receipt verifies
    against. Where a check is over-approximate by construction (the
    byte-level occurrence scans), its findings never blend into the
    verdict: the receipt carries a separate `conclusive` flag, and the
    subcommand exits `0` erased-and-conclusive, `3` erased-but-inconclusive,
    `2` not erased.
  - **Pins no longer share the append-only logs' inodes.** A pin hard-links
    the immutable files and *copies* `annotations.jsonl` and
    `tombstones.jsonl` at their manifested length. Hard-linking them let
    every later append edit the "backup" in place — a pin taken before an
    erasure inherited the erasure's settle record through the shared inode,
    so restoring it produced a store that recorded a deletion it did not
    contain.
  - **Erasure requires a new `admin` token scope.** `TRAZA_TOKENS` grows
    `admin:` alongside `rw:`/`ro:`; `POST /v1/erasures` refuses plain `rw`
    with a 403. Every collector holds a write token, and a credential minted
    to produce telemetry must not be the credential that destroys it.
    Payload subjects are canonicalized to lowercase hex before anything is
    resolved or recorded — an uppercase hash previously matched no stored
    reference and produced a green receipt over untouched content. Redaction
    markers are no longer counted as live payload references anywhere
    (protection sets, receipts): a marker records that content is gone.
  - **No MCP tool for any of it, deliberately.** Deletion is an HTTP verb
    behind the `admin` scope; the agent-facing surface stays read-only, so
    stored adversarial text has no destructive tool to actuate.

- **Derived LLM cost from a configured pricing table**, `--pricing FILE`.
  OpenTelemetry defines no cost attribute, so a span carries one only if its
  pipeline metered it — and most do not, which left stores that knew the model
  and both token counts reporting `$0.00` on every cost surface. Rates are USD
  per million tokens, keyed by exact model name or a `prefix*` pattern
  (longest match wins; an exact name beats every pattern; a bare `"*"` is a
  default). There is no built-in table and will not be one: prices move on the
  vendor's schedule, and self-hosted models have no public rate.
  - **A metered `llm.cost_usd` always wins**, and a span reporting only a
    total token count stays unpriced rather than being split by an assumed
    input/output ratio.
  - **Estimates are reported as estimates, by count rather than by amount.**
    Every cost-bearing row — `/v1/stats/llm`, `/v1/sessions`, and each bucket
    of `/v1/stats/series` — carries `cost_derived_usd` beside `cost_usd` plus
    `cost_metered_calls`, `cost_derived_calls` and `cost_unpriced_calls`. The
    dollars cannot carry provenance on their own: a zero-rate model is priced
    and adds nothing, a call nothing could price also adds nothing, and
    neither is distinguishable from a genuine measurement of zero.
    `cost_derived_calls > 0` means the total is an estimate;
    `cost_unpriced_calls > 0` means it is an undercount. The dashboard and MCP
    both mark an estimate `~`, show `—` rather than `0.0000` when nothing
    could be priced, and state the breakdown — and `analyze_cost` no longer
    claims cost is exact when a rate table contributed to it.
  - **Rollup sidecars record the fingerprint of the table they were folded
    under** (format v4), so editing the rates invalidates exactly the cached
    counters that would now be wrong instead of reporting last month's prices
    from a sealed segment forever. An empty table fingerprints to zero, so a
    store that prices nothing binds its sidecars exactly as before. A
    malformed pricing file refuses startup rather than being ignored.

### Changed

- **Ingest rejects an inadmissible tenant with 400, on both surfaces.** A
  tenant is identity: lowercase `[a-z0-9][a-z0-9._-]`, at most 64 bytes, or
  empty for the default. A misconfigured `traza.tenant` resource attribute
  fails the whole OTLP export loudly rather than being silently dropped —
  partialSuccess is for data the server chose to suppress, not for a defect
  the client must fix.
- **Cursor tokens carry a version byte.** The ordering key changed shape
  (tenant joined it), and a pre-tenancy token must parse as *invalid*, never
  as a plausible wrong position. Live cursors from before an upgrade get a
  400, which is what a stale cursor always deserved.
- **The span identity key on the wire and on disk is `$tenant`, not
  `tenant`.** A span's top-level namespace is open — unknown fields survive
  in the round trip — so a bare `tenant` is client data and must stay client
  data. The `$` sigil (as in `$payload`) is the format discriminator: bytes
  written before tenancy cannot carry `$tenant`, so a store read after an
  upgrade keeps its bare `tenant` values as data rather than promoting one to
  an identity no query selects and no erasure names. This replaces an earlier
  decode-time normalization that tried to tell legacy data from identity by a
  value's shape — which a valid-looking legacy value (`"acme"`) slipped
  through. The `?tenant=` read filter and the erasure subject's `tenant`
  field are closed namespaces and keep the plain name — but they, along with
  an annotation's and a dataset's `tenant`, now also **accept `$tenant` as an
  alias**, so a client that learned the span's spelling cannot silently
  misroute a score, a dataset, or — worst of all — an erasure to the default
  tenant. The span alone rejects a bare `tenant` as identity, because the span
  alone has the open namespace that makes it data.

- **Append-only files are digested and verified over their recorded prefix,
  exactly.** `digest_engine` recorded an append-only log's length from one
  instant and its hash from another (`sha256` over the live file), and
  `verify_against` hashed the whole file whenever the lengths happened to
  match — so an annotation or settle record appended mid-walk made a
  freshly published generation fail its own verification with a digest
  mismatch that was growth, not damage. Both sides now hash exactly the
  recorded length, which is what the checkpoint's concurrency story always
  claimed. Found by the erasure transition stress test; the race predates
  erasure and was reachable by any annotation landing inside a checkpoint's
  digest walk.
- **`wal_bytes` counts replayable work, not file bytes.** The log's constant
  preamble is excluded, so a reclaimed log measures zero. Both the
  `flush_wal_bytes` bound and the statistic mean the same thing — what a
  restart would redo — and a constant is neither replayed nor reclaimable.
- **The checkpoint's commit point is proven by SIGKILL**, not only
  deterministically. `tests/durability.rs` kills a real server mid-checkpoint
  at three named on-disk signals — the manifest directory appearing,
  `.CURRENT.tmp` existing between the staged write and its rename, and
  `CURRENT` itself changing — because those windows are single fsyncs wide
  and a stagger alone lands wherever the machine favours. Recovery must then
  answer with exactly one state: every acknowledged span present, the hot key
  at the newest acknowledged version, the live generation verifying clean
  over `GET /v1/verify`, a complete manifest behind whatever `CURRENT` names,
  and nothing staged surviving the sweep. Publishing `CURRENT` before the
  manifest is durable fails it.

  The staged-`CURRENT` signal exists because a slower CI runner reached that
  window when a fast machine never did — and a test that only covers a state
  when the machine happens to be slow is a test that passes for the wrong
  reason. Aiming at it beats depending on the weather.

  Recorded because the opposite would be a claim rather than evidence: the
  *other* ordering rule — reclaiming folded frames only after `CURRENT` is
  durable — was mutated too and did **not** fail. A checkpoint seals the
  buffer first, so the frames it reclaims are already in a durable segment,
  and recovery loads segments by walking the directory rather than by reading
  the manifest. That rule is defensive under today's engine and becomes
  load-bearing the moment segment loading is manifest-driven, which is what a
  replicated snapshot install wants. Invariant 12 now says so.

- **Checkpointing is never a side effect of a primitive.** It seals the write
  buffer, and expiry must not decide when to seal: `expire_before` deletes
  from every domain and stops, exactly as before. The deletion is durable when
  its domains are durable and *published* by the next checkpoint — one
  maintenance interval away, or immediately when a backup asks.

### Fixed

- **A pending whole-tenant erasure withholds its payload bytes from a scoped
  fetch.** A tenant erasure discovers its span-held references only as its
  purge walks them, so the mask's `payload_files` set fills after the mask
  already hides the tenant. A `GET /v1/payloads` under a bound credential now
  returns nothing the moment its tenant is masked — whether or not that exact
  reference has been enumerated — closing a window in which a planted crash
  state served the bytes an erasure was seconds from deleting. Freshly
  discovered references are also folded into the live mask as they are noted,
  so the operator path stops serving them mid-purge too.
- **The MCP `describe_store` "nothing to search yet" note follows the
  caller's own usage, not the store total.** For a tenant-bound caller the
  note was keyed on the store's `total_records`, so it appeared only on a
  globally empty store and vanished the instant any *other* tenant ingested a
  span — a co-tenant presence oracle across the isolation boundary the rest
  of the overview respects. It now reads the bound tenant's own row.
- **An explicit default-tenant scope reaches its own sealed payload.** The
  default (empty) tenant carries no attribute-index posting — that is what
  keeps single-tenant stores byte-identical — so a payload-reachability probe
  that trusted the posting found nothing for a `Some("")` scope once the span
  sealed. The probe now scans the records and lets the decoded tenant decide
  for the default tenant, exactly as `select_probe` already did.
- **Resolving last-write-wins no longer re-reads the store to prove keys were
  never replaced.** Every query-side supersede probe — the limited merge, the
  unlimited scan, and the fold behind the aggregation routes — now consults
  each newer segment's own key-hash set before paying an exact probe, the
  prefilter the analytics fold's exact path already ran; that fold now gates
  per segment too, so a key rewritten eleven segments later costs one probe
  rather than a walk across the ten between. On a store carrying superseded
  versions — a crash-recovered store before its first compaction is the
  canonical case — queries cost matches × segments × trace-width decodes,
  which is how a recovery query blew a 30-second deadline in CI. The new
  `traza_supersede_probes_total` counter is the observable: roughly one probe
  per superseded version actually held.
- **A pre-`$tenant` rollup sidecar is rebuilt, never believed.** Reserving
  `$tenant` changed what a bare `tenant` field decodes to, and a sidecar's
  key hashes are evidence only under the decoding they were computed with —
  trusting one across that boundary treated a stale membership miss as proof
  of absence and resurrected a superseded span. The rollup `SCHEMA_VERSION`
  is bumped, so the first read after upgrading rebuilds each segment's
  sidecar once; `tests/fixtures/pr50-tenant-identity`, sealed by the
  pre-reservation build, is the corpus that fails if a future decoding change
  forgets the bump.
- **Native ingest accepts an event's timestamp under the OTLP name.** A span's
  timestamps accepted `start_time_unix_nano`; its events accepted only
  `timestamp_ns`, so a client spelling both the way OTLP spells them had its
  **entire batch** rejected with a 400 naming a field it had supplied. Events
  now take `time_unix_nano`, `timestamp_unix_nano`, `time_ns` and `time` as
  aliases, and `attributes` defaults, so a named instant no longer needs an
  empty map to be legal. The failure mode this closes is quiet rather than
  loud: telemetry clients are conventionally fail-open, so the spans simply
  never arrived.
- **The span search's token column reads the current OpenTelemetry names.**
  It carried its own third copy of the semantic-convention precedence, which
  had drifted from `src/semconv.rs`: it resolved only the deprecated
  `gen_ai.usage.{prompt,completion}_tokens` and a `llm.usage.prompt_tokens`
  key Traza has never recognized. A span using the current `input`/`output`
  names — resolved correctly by the server and by the trace detail — showed a
  blank cell. It now uses the shared `llmUsage` helper, as everything else
  does.
- **Two toolbar actions on the span search worked in name only.** A local
  named `window` shadowed the global for the whole component, so
  `window.prompt` and `window.location` read `undefined` through the optional
  chains guarding them: saving a view never asked for a name and silently
  numbered every one `view N`, and "Copy as curl" emitted a hostless URL that
  curl refuses.

## [0.22.2] - 2026-08-12

### Fixed

- **The dashboard's rail reports the version that is actually running.** A
  hardcoded prop default shipped "0.19" on every install regardless of the
  build; the rail now takes its version from `ui/package.json` at build
  time.

### Changed

- **The README leads with the install and a picture instead of a wall of
  sections.** A real trace-browser screenshot (an agent swarm's waterfall,
  from the seeded corpus) sits above four install paths — release archive,
  container, crates.io, source — and the HTTP API table, library example,
  and design prose moved out to the documentation that owns them. A social
  preview card in the repo's own design system lives at
  `docs/assets/social-preview.png`.

- **The release pipeline validates every channel before it publishes to
  any.** v0.22.1 proved the previous shape only failed loudly at the end:
  the crates.io token was unset, and the GitHub release and container
  image had already published by the time the pipeline said so. Preflight
  now runs the full gate on both platforms plus the MSRV check, verifies
  the crates.io token exists, and runs `cargo publish --dry-run --locked`
  — all before a single artifact is built — and the GitHub release moved
  to the end, behind the crates and container publications it used to
  precede. Three registries cannot be atomic; prerequisites can be checked
  before touching any of them.
- **The grouped-merge crash test's deadline is a failsafe, not an
  oracle.** Its 30-second query deadline doubled as the duplicate-version
  check, which turned shared-runner weather into red durability legs. The
  deterministic assertions after each query — a hot key surviving other
  than exactly once, a stale version outranking an acknowledged one, a
  batch coming back short — were the real oracle all along; the deadline
  is now 300 seconds and its message says what it is.
- **`ci.sh` audits the shipped dashboard's dependencies.**
  `npm audit --omit=dev --audit-level=high` gates the merge bar;
  development-tool advisories print without blocking. The two open
  advisories (nanoid, postcss — both development-only) are resolved in
  the lockfile.
- **Workflow actions are pinned to commit SHAs**, with the tag each pin
  was resolved from in a comment, and Dependabot groups cargo, npm, and
  actions updates into one weekly PR per ecosystem. The changelog's
  footer now carries the full compare-link lineage, every release back
  to v0.1.0.
- **Future-facing documentation left the repository.** `docs/roadmap.md`,
  `docs/ha-design.md`, and `docs/generations-design.md` moved to the
  private planning workspace and were removed from the repository **and
  its git history** — the repo documents what Traza does; where it is
  going is tracked elsewhere. This is the second history rewrite (the
  first predates going public), and it re-hashed every commit and
  re-pointed every tag, including the ones the 0.22.0 note below could
  still call bit-identical when it was written. Stated here for the same
  reason as last time: a lineage that quietly changed under its tags
  would be worse than one that says so. Published GitHub release artifacts
  still contain the files as they were at their release; the crates.io
  versions that carried them (0.22.0, 0.22.1) were deleted outright within
  the registry's deletion window, and this release is the first published
  from the purged tree.

## [0.22.1] - 2026-08-07

### Fixed

- **The release archive's dashboard now sits where the server actually
  probes.** v0.22.0 packaged the UI at `ui/dist` inside the archive, but
  the binary-relative search — the packaging convention the source itself
  documents — looks for `<binary dir>/ui`. Launched from inside the
  extracted directory, the working-directory fallback hid the bug; launched
  from anywhere else, `/` was a 404. The archive now places the build at
  `ui/`, and every release archive is smoke-tested **from an unrelated
  working directory** — dashboard 200, span accepted and read back —
  before anything publishes, so this class of bug cannot ship twice.
- **The README's container quickstart no longer discards the token it
  mints.** The old one-liner generated `TRAZA_TOKENS` inline, so the server
  started and the reader had no way into their own dashboard. The token now
  lands in a shell variable and is echoed before use.

### Added

- **`--version` / `-V`** on `traza-server`, printing the crate version.
- **Third-party notices ship with the artifacts.** Release archives and
  container images now carry `THIRD_PARTY_NOTICES.md`: the MIT and OFL
  material for React, Inter and JetBrains Mono in the dashboard build, and
  the dual-licensed Rust crates linked into the binary.
- **The release pipeline fails closed.** A preflight job refuses a tag
  whose version disagrees with `Cargo.toml`, `ui/package.json`, or this
  file, and runs the full `./ci.sh` gate on the tagged commit before
  anything builds; the crates.io job fails loudly when the registry token
  is absent instead of skipping green. A new CI job checks the whole tree
  on the advertised MSRV, holding `rust-version` to being true rather than
  asserted — and its first runs proved the asserted 1.70 false twice over:
  the locked dependency tree requires 1.71, and the test suite uses
  `File::set_times`, stabilized in 1.75. `rust-version` now says 1.75,
  because that is the number the gate can hold.
- **`SECURITY.md`** (private vulnerability reporting, supported versions,
  and the threat model in one paragraph), a code of conduct, issue and PR
  templates, and Dependabot coverage for Cargo, npm, and GitHub Actions.

### Changed

- **The container runs as uid 65534**, with `/data` shipped owned by that
  uid so a named volume inherits it — a bind-mounted data directory must be
  writable by 65534 — and images carry OCI source, version, and license
  labels.

## [0.22.0] - 2026-08-07

### Added

- **An install, not a build.** Tagged releases now ship what the README used
  to ask you to compile:
  `traza-<version>-{linux-x86_64,linux-aarch64,macos-aarch64}.tar.gz`, each
  carrying the `traza-server` binary (musl-static on Linux — no libc to
  match) with the dashboard already built in `ui/dist`, alongside
  `SHA256SUMS` and GitHub build-provenance attestations
  (`gh attestation verify`). The same binaries become the container image,
  `ghcr.io/toshish/traza`, built `FROM scratch` — an engine with two
  dependencies does not need a distribution underneath it. A non-loopback
  bind still refuses to start without `TRAZA_TOKENS`, in the container as
  anywhere else. The tag also publishes to crates.io when the registry
  token is present.
- **The merge bar runs in public.** `./ci.sh` — the same script, no drift —
  now runs on Linux and macOS for every push and pull request, and the
  README carries the badge. Until this release the script had only ever
  gated one platform: the laptop it was written on.

### Fixed

- **The dashboard now renders trace media wherever the trace actually put
  it, instead of only the one lucky shape it recognized before.** Found via
  a production vision-review trace whose image showed as an empty frame: the
  conversation view understood only `data:`/`http(s):` strings in a part's
  `data`/`uri`/`url`, which missed most real emitters and — worse — poured
  every other locator into the transcript as text (a bare-base64 screenshot
  rendered as eight thousand lines of base64). The messages panel and the
  trace inspector's payload modal now recognize, with tests: bare-base64
  `data` lifted into a `data:` URI via the declared MIME type; Anthropic
  `source.{base64,url,file}` parts; Google GenAI typeless `inline_data` /
  `file_data` parts; OpenAI `input_audio`, `image_url`-object, and `file`
  parts; MCP-style tool-result content lists (screenshots included);
  `width`/`height` metadata; and parts whose bytes were never captured,
  which now say so with the emitter's `unavailable_reason` instead of
  presenting an empty frame that reads as a rendering bug.

- **Offloading no longer demotes media to a JSON dump.** A
  `gen_ai.{input,output}.messages` attribute past `--payload-threshold-bytes`
  becomes a payload reference, and the old conversation view could only show
  it as preview text — "load full" printed the raw JSON, megabytes of base64
  included. The payload body is the original messages JSON, so the
  conversation view now fetches it back (automatically up to 4 MiB, when the
  turn nears the viewport; on demand above that, content-addressed and cached
  across screens) and renders the parsed turns — an offloaded text-to-speech
  span plays its audio again. The trace inspector's payload modal gained the
  same understanding: message payloads render as turns, media payloads as
  media, anything else as code, with the literal bytes one `raw` toggle away.

- **Every MCP tool now advertises `ToolAnnotations`.** The catalog carried
  names, descriptions and schemas but no `readOnlyHint`, `destructiveHint`,
  `idempotentHint` or `openWorldHint` — and absent hints are not neutral. Each
  defaults to the pessimistic answer, so nine read-only tools were advertised
  as potentially destructive and open-world. A host that gates on that asks for
  approval on every call; one running non-interactively, with nobody to ask,
  cancels them, which is what a Codex run did. The nine readers now declare
  `readOnlyHint: true` with `openWorldHint: false`, and `record_annotation`
  declares an additive, non-idempotent write. Tool nature is a required
  argument of the builder rather than a step that can be skipped.

- **`describe_store` reports the server version.** `initialize` carried it in
  `serverInfo`, but a host reads that once and need not pass it to the model,
  so an agent asked which Traza it was talking to correctly answered that it
  could not tell. The overview block — and the `traza://store/overview`
  resource that shares it — now names the version.

### Changed

- **The measurement records moved out of the repository root into
  [`docs/benchmarks/`](docs/benchmarks/)**, and are named for what they
  measure rather than shouted: `BENCHMARKS.md` is now
  `docs/benchmarks/canonical-corpus.md`, and `INGEST-BENCHMARK.md`,
  `STORAGE-BENCHMARK.md`, `QUERY-BENCHMARK.md` and
  `INDEX-MEM-BENCHMARK.{md,json}` are `ingest.md`, `storage.md`, `query.md`
  and `index-memory.{md,json}`. Six SHOUTING files at the root read as project
  furniture on the level of README and LICENSE; they are generated
  documentation, and they belong with the documentation that cites them.
  Every benchmark binary writes to the new path, and `docs/README.md` now
  indexes all five records rather than two. Entries below this one name the
  files as they were called at the time.
- `query-bench` publishes its record only for the canonical configuration, the
  rule `bench` already held itself to. Now that the record is a published
  document, an experimental corpus or client count would otherwise have
  overwritten the committed numbers with an answer to a different question.

- **The advertised tool catalog is smaller than before the annotations were
  added**: 14,895 → 14,296 bytes, with 628 bytes of new annotations inside
  that. The four accepted time formats were restated on ten properties while
  the `initialize` instructions already spell them out once per session; that
  repetition and some restated prose are gone. Every semantic warning stays —
  word-not-substring, status-is-not-an-attribute, a missing key is kept, and
  the ranking ceiling that sends a caller to `slowest_spans`.

- **The published history was rewritten once, before the repository went
  public**, to remove an internal live-deployment analysis that had no
  business in a public tree. Tags `v0.1.0` through `v0.20.0` are
  bit-identical to the private originals; commits from the `v0.21.0`
  release onward carry new hashes. Stated here because a lineage that
  quietly changed under its tags would be worse than one that says so.

## [0.21.0] - 2026-07-31

### Added

- **The write buffer now bounds itself in time and self-corrects observed key
  shadowing, instead of trusting volume thresholds an idle store never
  reaches.** Found by measuring a real deployment: a trickle workload
  (~150 spans/day against the 10,000-span flush threshold) had parked upserted
  keys in the write buffer for **36 days**, which held ~12 MB of log a restart
  would replay and — far worse — disqualified every segment holding those keys
  from answering `stats/llm`/`sessions` out of its rollup, for the entire
  wait. Two new bounds, both mechanism-triggered and both on by default:

  - **`--max-buffer-age-seconds` (default 300, `Config::max_buffer_age`)** —
    the oldest buffered span's wait is now a seal trigger, checked on the
    ingest path and by the new `Store::maintain_buffer`, which `traza-server`
    drives from its maintenance tick. Under real traffic the volume thresholds
    fire first and this never does; `traza_segment_seals_age_total` says when
    it did.
  - **A corrective merge on observed segment-key shadowing
    (`--no-shadow-seal` to disable, `Config::shadow_seal`)** — the analytics
    fold already decides, per segment, whether a rollup is safe or the same
    `(trace_id, span_id)` exists in a newer segment; that decision now
    latches a flag instead of being discarded. Maintenance converts the latch
    into a merge of the shadowed tail run, chosen by a scan that reads only
    cached or sidecar rollups — never a decode — and bounded by
    `--compaction-max-segment-bytes`. The merge rides the existing journaled
    machinery: same permit pin, same contiguous id claim, same rollup
    handover. Three deliberate restraints, each the result of adversarial
    review: buffer-caused shadowing does not latch (a merge cannot retire a
    key a client is still updating — the age bound handles it); a successful
    merge cools the pass down for 15 minutes, so a workload that re-poisons
    after every merge gets a bounded rewrite rate; and a pass that finds
    nothing mergeable backs off exponentially to hourly instead of
    re-scanning every interval. Inert when compaction is disabled, which
    owns all merging. A latency threshold is deliberately NOT the trigger:
    the latch fires exactly when a query had to decode instead of using a
    rollup, and stops the moment the duplicates are merged away. Counter:
    `traza_shadow_merges_total`.

  Measured end to end on a copy of the deployment that motivated it (32k
  spans, 46 MB, 3 segments, 1,368 shadowed keys): one seal plus one shadow
  merge leaves a single deduplicated segment, after which whole-corpus
  `stats/llm` answers from rollups in **0.55 ms** against ~170 ms before the
  pass — and against ~20 s on the pre-rollup engine the deployment was
  running. `/v1/stats` now reports `buffer_age_seconds` so a scheduler that
  stopped calling `maintain_buffer` is visible.

- **A storage-cost benchmark (`storage-bench`) and an honest comparison
  against OpenObserve's published table**
  ([docs/storage-comparison.md](docs/storage-comparison.md),
  [STORAGE-BENCHMARK.md](docs/benchmarks/storage.md)): the same eight metrics
  OpenObserve computes against Elasticsearch, measured for Traza's segment
  format, including the configurations where Traza loses the comparison and
  why.

- **An MCP server, embedded in `traza-server`: `POST /v1/mcp`.** Traza's read
  path had two consumers — the dashboard and `curl` — and the one its own
  vision implies was missing: the agent that produced the traces. `--mcp`
  serves the [Model Context Protocol](docs/guide/mcp.md) from the same binary,
  same port, same auth gate, with **no new dependency** (JSON-RPC is
  `serde_json`; the transport is the HTTP server that already exists) and no
  engine change. Tool handlers call `Store` directly rather than looping back
  through the socket, which is why `tests/mcp.rs` can drive the whole surface
  with no listener in the process.

  **Ten tools, shaped like questions rather than routes**: `describe_store`,
  `search_spans`, `get_trace`, `list_sessions`, `get_session`, `top_failures`,
  `slowest_spans`, `analyze_cost`, `get_payload`, and `record_annotation`
  behind two gates. A mechanical translation of the route index would have been
  nineteen tools a model cannot tell apart. `describe_store` exists because an
  agent that guesses a service name gets an empty result indistinguishable from
  "nothing is wrong" and reports that everything is fine.

  **Results are bounded in tokens, not rows.** One LLM span with a prompt and a
  completion is 20–50 KB, so the REST default of `limit=100` would be an
  unusable call that also costs money to fail: span tools default to 20, stored
  content is omitted unless asked for, every result is capped by
  `--mcp-max-result-bytes`, and **every truncation is stated along with the
  argument that would have narrowed it** — a silently shortened answer gets
  reported by a model as a complete one.

  **Stored span text is treated as untrusted.** It is confined to a delimited
  block with a preamble saying so, control characters are escaped so a newline
  cannot forge a row, the delimiter is neutralized so a value cannot close the
  block early, and it never reaches a tool name, description, or error message.
  The load-bearing mitigation is architectural: the server has no fetcher, no
  shell, no filesystem write and no outbound network path, so an injected
  instruction has nothing to actuate.

  Also: five resources and three URI templates (`traza://trace/{trace_id}` and
  friends, so a trace id in a tool result is something a host can attach), four
  prompts that each carry the live store overview as an embedded resource, a
  `traza-server mcp --url` stdio bridge for clients that launch a subprocess,
  and a dashboard **MCP** screen that reads the live surface from the running
  server rather than describing what the build believes it to be.

- **Per-tool authorization on the MCP route.** Every other route maps scope to
  HTTP method, which is right where the method *is* the operation. MCP tunnels
  reads and writes through one `POST`, so the method rule would either lock
  every `ro` token out of a read-only surface or hand every caller the write
  scope. `AuthConfig::scope_for` authenticates without applying it, and the
  endpoint authorizes per tool: `ro` reaches every read tool, and
  `record_annotation` needs `rw` *and* `--mcp-annotations`. A tool the token
  cannot call is never advertised to it — a model shown one calls it, reads the
  refusal as transient, and retries.

- **A `mcp` route class in `/v1/metrics`.** One `POST /v1/mcp` can be a lookup,
  a search or a whole-store rollup depending only on the tool named in the
  body; filed under `other` alongside static assets it would describe neither.

### Fixed

- **The MCP endpoint's DNS-rebinding defence trusted a header the attack
  controls.** `Origin` was accepted whenever its authority equalled the
  request's `Host` — which is exactly what a rebinding request supplies, since
  the attacker owns the name and the browser sends theirs in both headers. A
  page on any domain could drive a loopback Traza and read the whole store.
  Origins are now checked against loopback plus an operator-supplied
  `--mcp-allowed-origin` allowlist, and nothing else the request carries is
  consulted; the `Host` header is no longer read at all, so the comparison
  cannot be reintroduced by accident.

- **`list_sessions` ranked a page instead of the population.** `order_by=cost`
  fetched the most recent sessions and re-sorted those, so an expensive session
  outside the recency window was not lower down — it was absent. Ranking moved
  into `Store::sessions`, which already materializes every session in the
  window, so the comparator change costs nothing and the answer is over the
  whole population.

- **`structuredContent` escaped `--mcp-max-result-bytes`.** Only the text block
  was clamped, so one long stored identifier produced an 83-byte text block
  inside a 100 KB result under a 1 KiB ceiling. Text and structured content are
  now budgeted and trimmed together; the ceiling bounds the tool result, not
  one field of it.

- **Impossible timestamps became different valid ones.** `2026-02-31` resolved
  to March 3rd, `2026-07-27T99:99:99Z` to July 31st, and a `+99:99` offset was
  accepted — each silently substituting a window nobody asked for, over which
  the answer looks correct. Month lengths, leap years (century rule included),
  time-of-day and offset ranges are validated before conversion.

- **Empty results violated the schema their tool advertises.** `list_sessions`
  and `analyze_cost` returned text-only results for an empty store or window,
  while both declare required `outputSchema` fields — so a validating client
  would reject the most routine answer either tool gives. They now return
  `{"sessions": []}` and `{"group_by": "…", "rows": []}`, and the path that
  trims rows to fit keeps the structured half rather than dropping it.

- **Small ceilings were exceeded by the JSON envelope.** `--mcp-max-result-bytes`
  was applied to the text block, then the result was wrapped — so a 256-byte
  ceiling shipped 286 bytes. The ceiling is now enforced on the whole
  serialized result at the single point every tool result passes through, and
  a ceiling below 1,024 bytes is refused at startup, because beneath that no
  result can both fit and conform.

- **The dashboard generated a stdio configuration that could not work over
  TLS.** It interpolated `window.location.origin` into `traza-server mcp
  --url`, which the bridge refuses for `https://`. On a secure origin it now
  asks for the plaintext endpoint instead of emitting a copy-ready snippet that
  fails.

## [0.20.0] - 2026-07-27

### Added

- **`GET /v1/tail` streams spans as they are admitted**, as server-sent events.
  This is the only route ordered by admission rather than event time, and that
  is why it exists: `/v1/spans?since=` answers "what STARTED after T", which is
  a different question from "what is arriving". A span running longer than a
  client's polling interval starts before the watermark and arrives after it, so
  the old polling tail dropped it permanently — not late, never. Admission order
  comes from a bounded in-memory ring, so it costs no disk, no segment format
  version, and no branch in the query path. Every `/v1/spans` predicate works,
  content search included. An event-time bound is refused with `400` rather than
  ignored, at the HTTP surface and in `Store::tail_after` alike.

  Bounded by `--tail-ring-spans` (count) and `--tail-ring-bytes` (memory),
  whichever binds first. The byte bound is the one that matters: the ring owns
  whole spans once a seal drops them from the write buffer, and an LLM span
  carrying a prompt is orders of magnitude larger than one carrying a status
  code. Residency and both bounds are reported at `/v1/metrics.json` under
  `tail_ring`.

  The ring is bounded by **bytes as well as count**, and the byte estimate
  counts structure and retained capacity, not only text — a `Value` slot per
  element and per map entry, and `capacity()` rather than `len()` for every
  collection. Counting text alone left the ceiling bypassable by shape; counting
  logical length left it bypassable by a `Vec` grown large and truncated, which
  keeps its whole allocation.

  The screen itself is under test now (`ui/src/views/TailScreen.test.jsx`,
  jsdom + Testing Library), which found four defects nothing else could reach:
  **resume produced no rows** — `setRows` was given an updater that read
  `buffer.current`, and the buffer was cleared before React ran it; **every
  keystroke in the service filter opened a new stream**, eight connections for
  "checkout", now settled over 250 ms; **row keys included the array index**, so
  prepending a batch changed every key and rebuilt all 300 rows per frame, now a
  client-assigned id; and **a full pause buffer discarded its overflow
  silently**, now counted and shown. The rate window is bounded by samples as
  well as by duration.

  A gap is a **visible** discontinuity even when the server cannot count it. A
  cursor from before a restart is not comparable to the new numbering, so
  `missed` is `null` rather than a number, and the client shows the break
  without inventing a count — tracking counted and uncounted breaks separately,
  so "unknown, then 5" reads as `5+` rather than as an exact 5. `backfill` survives a gap unchanged, including
  `backfill=0`.

  A streamed span has been **acknowledged**: entries reach the ring only after
  the ingest succeeded, past the log's fsync and past the seal `flushed`
  promises. The tail is bounded and may gap, but it never shows a span the store
  did not accept.

  A subscriber that falls further behind than the ring retains gets a `gap`
  frame carrying a count and **no position**: the dropped entries are exactly
  the ones no longer addressable, and no query can name an admission range. The
  stream restarts at the live edge and the client rebuilds from there.

  The dashboard's Live tail consumes it over one held connection. An idle tail
  went from roughly forty empty round trips a minute to zero.

### Fixed

- **Aggregations no longer block ingest.** `fold_spans` held the writer and
  segment locks for the length of a full-corpus scan, so any series, duration,
  failure or slowest query stalled writes until it finished — and the Overview
  screen starts four at once. It now folds against a snapshot: one copy of the
  bounded write buffer, one reference per segment, locks released before the
  scan. That also makes an aggregate a reading of one instant rather than of a
  moving store. Guarded by `tests/fold_concurrency.rs`, which fails against the
  previous implementation.

- **Three arithmetic overflows reachable from valid requests.** A span with
  `end_time_ns = u64::MAX` panicked the duration histogram's top bucket bound;
  `until=u64::MAX` overflowed the series bucket width; `limit=u64::MAX`
  overflowed the tail's allocation. Debug builds panicked and closed the
  connection with no response; release builds would have wrapped, producing
  wrong series math and an effectively unbounded limit. All three now use
  checked or saturating arithmetic, with endpoint caps where a bound is also
  the right policy.

- **Failure grouping was unbounded in memory.** Every distinct
  `(service, name, status)` allocated a duration histogram and `limit` applied
  only after collecting them all, so high-cardinality error text — an id in a
  span name — could reach gigabytes before returning twenty rows. Bounded at
  4,096 signatures, with `spans_untracked` reporting what the bound excluded.

- **The dashboard could state a durability guarantee the server never made.**
  Both the Server and Overview screens printed the `wal` sentence — "survives a
  kill-9, a panic, or an OS crash" — unconditionally, and Overview read a
  `durability` field `/v1/metrics.json` did not have, defaulting it to `wal`.
  A `buffered` server, which promises the opposite, was described as durable.
  The field is now served, and the wording is derived from it in one place.

- **Drag-to-zoom silently reverted.** An absolute window serialized into the
  hash as `t=""` and read back as the `1h` default, so every brush selection
  snapped back to the last hour. Absolute ranges now round-trip as
  `t=<since>-<until>`, and a malformed one falls back instead of becoming a
  range of nonsense.

- **Live tail could skip spans permanently.** It advanced its watermark to
  `max(start_time_ns) + 1` and ignored `next_cursor`, so when more spans shared
  the last timestamp of a page than the page held — routine for an SDK-batched
  flush — the remainder were never returned. It now drains by cursor, and
  overlapping ticks are prevented rather than deduplicated after the fact.

- **`AbortSignal` did not cancel the request.** The shared coalesced request
  was started with no signal, so aborting rejected the caller's promise while
  the fetch, connection and server-side scan continued — the opposite of the
  intent on a screen that re-queries per keystroke. Subscribers are now
  reference-counted: one leaving cancels nothing, the last one cancels the work.

- **A screen labelled "live" never refreshed.** Overview resolved its window
  once at mount and polled nothing, so every figure was frozen at the moment
  the tab opened while the live dot pulsed over it.

- **Failure shares were computed against truncated data.** Both screens summed
  the groups they had been sent — a page cut to a limit — and used that as the
  denominator, inflating every signature's share of "all failures". The API now
  returns `total`, counted before truncation.

- **A series window near the top of the `u64` range still panicked.** The
  previous round fixed the bucket width and left the bucket *starts* unchecked
  three lines below it, so `since + width * index` overflowed for any window
  close to `u64::MAX`. The saturating ceiling was also one nanosecond short per
  bucket on a full-range window, leaving the last bucket ending before the
  window did. Starts are saturating and clamped to `until`; the ceiling is
  quotient plus non-zero remainder.

- **The live tail stranded bursts one page budget further out.** Carrying the
  cursor only *within* a tick moved the threshold from 200 rows to 1,000 rather
  than removing it: a burst larger than the budget was still abandoned, and the
  watermark cannot rescue it because every span in an equal-timestamp burst is
  `>= since`. An unfinished cursor chain now survives the tick that could not
  finish it, and the watermark advances only once the chain is exhausted. The
  polling state machine moved to `ui/src/lib/tail.js` so it is testable without
  a DOM — which is where both of its bugs were actually found.

- **Overview compared the wrong periods.** One selected window was split down
  the middle, so "24h" showed the last twelve hours against the twelve before
  them while the label said "previous 24h" — and the failure and model cards
  queried the full 24 hours, so three cards on one screen described three
  different spans of time. Two full periods are resolved now: the series covers
  both with the midpoint exactly on the boundary, and the other cards are
  scoped to the current period.

- **A settling request could evict its own replacement.** The in-flight map was
  cleared unconditionally in the finalizer, so an aborted request finishing
  after a fresh one had taken its slot deleted the live entry and made the next
  caller open a third. Eviction is identity-checked.

- **Overview's refresh was partial.** The tick re-ran the window-dependent
  reads but sessions and metrics kept empty dependency arrays, so "Server, in
  its own words" stayed frozen under a live indicator.

- **A paused tail buffered duplicates.** `since` is inclusive and the paused
  path skipped the primary-key check, so a quiet page was re-buffered until it
  filled. Both paths share one dedupe now.

- **A drained equal-timestamp burst replayed forever.** The dedupe set was
  evicted by size, but a burst sharing one timestamp cannot advance the
  inclusive watermark, so its keys are needed on every later poll: 1,250 spans
  at one timestamp cycled 1000, 250, 1000, 250… indefinitely. The set now
  retains exactly the keys ON the watermark and prunes only when it moves —
  bounded by one timestamp's membership, which is the minimum that is correct.
  The previous test stopped the moment the drain completed, which is precisely
  where the replay began.

- **A filter change could admit rows from the previous filter.** Replacing the
  tail's state and clearing the screen does not un-send a request; an in-flight
  poll still resolved and appended its old-filter rows. Responses are checked
  against a generation token.

- **Overview's period p95 was the worst bucket's p95.** `max(bucket.p95)` is
  not a percentile of a period, and one sparse slow bucket dragged it into
  seconds while the true figure sat in milliseconds. Each period now reads a
  duration histogram folded over the whole period.

- **The top model's spend share used a truncated denominator.**
  `/v1/stats/llm?limit=6` returns six rows, and their subtotal was the
  denominator, so the share inflated by whatever the limit left out. The
  series' total cost is the honest one.

- **Resuming a paused tail reversed its chronology.** The buffer is already
  newest-first, so reversing it handed back an oldest-first block.

- **Query cost under-reported content pruning.** Both search paths incremented
  the process-wide counter but not the per-query `segments_pruned`, so a
  content-narrowed search understated the work it had avoided.

### Changed

- **The segment format collapsed to a single version, numbered 6.** It had
  grown by appending header fields behind `if version >= N` gates, and each gate
  turned a field into an `Option` that every reader downstream had to treat as
  "unknown, therefore assume the worst": a segment whose timestamp range could
  not be read had to be scanned by every time-bounded query, and a second
  attribute-index decoder existed only to read the encoding that predated
  digests. `MIN_READABLE_VERSION`, `HEADER_LEN_V2`, `HEADER_LEN_V3` and the
  legacy decoder are gone; `timestamps` and `content` are plain values, so the
  pruning path no longer carries a case where it cannot prune.

  The failure names **one commit** that reads every superseded format
  (`traza::LEGACY_SEGMENT_READER`), not a per-version release. A store
  accumulates segments in whichever format was current when each was sealed, so
  one directory can hold several at once, and the release that reads the oldest
  cannot read the newest.

  **Upgrading makes existing stores unreadable.** Versions 2 through 5 were
  written by tagged 0.x releases — 2 in v0.16/v0.17, 3 in v0.18/v0.19, 5 up to
  the previous commit — so this is a real on-disk break for anyone running
  those, not a cleanup of formats that never escaped. `Store::open` refuses such
  a segment rather than misreading it, names the file, and points at the
  migration: back up the directory, open it with the release that wrote it,
  `GET /v1/export`, and re-ingest. The README's pre-1.0 terms permit this —
  "on-disk formats may change between 0.x versions" — but permitted is not the
  same as costless, and the work falls on whoever has data.

  The numbering deliberately does not restart. Removing compatibility *code* and
  reusing compatibility *identifiers* are different acts: a header declaring "2"
  must never be ambiguous between the layout v0.16 wrote and some later one, so
  1 through 5 stay spent and the single readable format is 6.

  The version word stays for the same reason it always should have. Two bytes
  per file, and it is the difference between refusing to open a format this
  reader does not know and parsing its header at these offsets — which yields
  section bounds that pass every validation check while addressing the wrong
  bytes.

  **This is a one-time exception, not the policy.** From v6 onward a format bump
  ships with a migrator: the runtime still reads one canonical format, and the
  conversion from the previous one lives outside the query path rather than in
  it. See [the segment format doc](docs/segment-format.md) for the rule. Doing
  it here would have meant resurrecting the decoders this change deletes, for
  stores that do not exist.

- **The logo is the revised mark from the design system.** The bars gained a
  stem, so the mark resolves as a lowercase "t" rather than four unanchored
  rows. It is a component now (`ui/src/components/Logo.jsx`) rather than SVG
  inlined per site, defaulting to `currentColor` because the design system is
  explicit that an img-referenced SVG cannot inherit page color and the
  reversed lockup depends on it. The wordmark's tracking was a step off the
  brand card (-0.01em against -0.02em). The favicon now uses the three-bar
  variant the system specifies for 16px, where the four-bar mark loses its
  rows.

- **Segment format v4: the attribute index is keyed by digest, not by value
  text.** Through v3 the index held every distinct attribute value resident,
  in full, for the life of the segment. For enum-shaped attributes that costs
  nothing; for the data Traza exists to store it was fatal — an indexed
  `gen_ai.prompt` is kilobytes, every value is distinct, and the resident
  index therefore grew to roughly the size of the corpus text. A store that
  reads records from disk on demand specifically so it can outgrow RAM was
  pulling the largest part of each record back into RAM through its own index.

  v4 interns attribute keys and replaces each value with a 128-bit digest, so
  an entry costs the same for a status code and for a page of text. Measured
  before and after on the same machine and command, 256 MiB of all-distinct
  text: **391 MiB → 21.6 MiB** RSS on open at 2 KiB values, **439 MiB →
  77.5 MiB** at 512 B. The 512 B cell is dearer because it holds four times as
  many spans — cost tracks cardinality now, which is the point.

  A digest probe returns candidates, so `attribute_posting_offsets_ref` is now
  `attribute_candidate_offsets` and `Segment::query_attribute` verifies. At
  128 bits nothing collides naturally, which means no ordinary test can tell a
  verifying reader from a trusting one; the acceptance target forges the
  collision instead.

  v2 and v3 segments still open — their values are hashed and discarded at
  open time, so an existing store gets the smaller steady state without being
  rewritten.

### Added

- **Content search: `GET /v1/spans?content=refund`.** Finds spans by the words
  in their text — string attributes, nested message arrays, event attributes
  and event names. Segment format v5 carries a Bloom filter over the words in
  each 128-record block, stored bit-sliced (one row per bit position spanning
  all blocks) so a probe reads tens of bytes per segment instead of every
  block's whole bitmap. Only a per-segment summary filter is resident, capped
  at 32 KiB, so the cost scales with segment count and not with text volume.

  Measured against the same corpus with the index off, 200,000 spans holding
  145 MiB of text: a word in one span **1.48 ms vs 1,258 ms (849x)**, two words
  **0.74 ms vs 1,156 ms (1,554x)**, a word in no span **0.008 ms (146,000x)**,
  and a word in nearly every span **1.0x** — with no selectivity to buy there
  is nothing to win, and that row is published alongside the others. Disk cost
  +0.1%, resident ~2 KiB per segment.

  **It is word matching, not substring matching, and not phrase matching.**
  `refund` does not match `refunds`. That is a soundness requirement rather
  than a limitation of effort: a word index cannot over-approximate a
  substring query, so driving one with it would skip spans that should have
  matched — a wrong answer instead of a slow one.

  `--no-content-index` / `Config::content_index` turns it off; content search
  still works, by scanning.

- **`traza_segments_pruned_by_content_total`** and
  **`traza_records_admitted_by_content_total`** in `/v1/metrics`. A content
  index that stops pruning returns byte-identical results and simply gets
  slower, so these counters are the only way to see it happen.
- **`status=` and `not_status=` on span search.** Every aggregate in the store
  counted errors from `Span::status`, but nothing could select on it: the
  filter walked `span.attributes` only. The natural-looking `attr.status=error`
  therefore matched an *attribute* most instrumentation never writes and
  returned an empty array indistinguishable from "no errors" — and the docs
  used exactly that expression as their motivating example. "Show me the
  failures" is now a query the API can answer.

- **Cursor pagination on `GET /v1/spans`.** The engine has had `query_after`
  and a total order for as long as export has existed; the HTTP surface never
  exposed it, so clients paged by re-requesting with a larger `limit` — which
  re-reads and re-sends every row already in hand. Responses now carry
  `next_cursor`, and a short page carries `null` rather than a token that could
  only return nothing.

- **Per-query cost in the span-search response.** `cost.elapsed_ns`,
  `segments_examined` and `segments_pruned`, counted per query rather than
  sampled from the process-wide counters, which race under concurrent readers
  and cannot be attributed. Traza's argument is that filtered search is cheap;
  this is the evidence rather than the assertion.

- **`GET /v1/stats/series`, `/duration`, `/failures`, `/slowest`.**
  Aggregations that answer "where should I look": volume, errors, tokens, cost
  and duration percentiles bucketed over a window; the duration distribution;
  errors grouped by `(service, name, status)` with first/last seen and an
  example to open; and the slowest matching spans ranked across the whole match
  set. All four fold in one pass and constant memory through a new
  `Store::fold_spans`, so the cost of an aggregate is proportional to the
  answer rather than to the corpus — a twenty-bucket histogram no longer
  materializes a million spans to produce it.

- **`GET /v1/metrics.json`**, and **request latency split by route class**
  (`ingest`, `lookup`, `search`, `stats`, `other`). One blended histogram over
  ingest and search described neither: they differ by orders of magnitude. Also
  adds `traza_uptime_seconds` and `traza_http_responses_{2xx,4xx,5xx}_total`.

- **`traza_wal_lock_wait_ns_*` and `traza_wal_write_syscall_ns_*`** in
  `/v1/metrics`, splitting the log append into waiting for Traza's log lock
  and the `write` itself. `traza_wal_write` covered both, which made the
  dominant remaining in-lock cost unattributable: it looked equally like lock
  contention and like slow I/O, and those want opposite fixes.

  The split settled it. Traza's log lock measures **zero** wait; the time is
  the syscall. And the syscall is not slow on its own — the same append costs
  **0.076 ms at concurrency 1 and 1.778 ms at concurrency 8**, a 23x
  difference in one syscall with no lock involved, because an `fsync` in
  flight blocks concurrent `write` calls to the same file inside the kernel.
  Widening `--wal-commit-window-us` from off to 2,000 cut fsyncs 40% and the
  mean append 57%. Documented in the configuration reference: the lever for a
  large `wal_write` is fsync frequency, not the engine's locking.
- **`INDEX-MEM-BENCHMARK.md` and `INDEX-MEM-BENCHMARK.json`** — the committed
  measurement record behind the memory figures, with commit SHA, machine,
  timestamp, load average per row, `compacted_away`, and the raw per-cell
  results. The prose numbers were previously traceable only to a binary.

### Changed

- **Latency percentiles are now publishable: at most 6.25% high, never low.**
  The stage histograms used plain power-of-two buckets, so a reported
  percentile could be **2x** the truth — fine for ranking stages against each
  other, and this project's own monitoring guide said in as many words not to
  publish them as request latencies. Each octave is now split into sixteen even
  steps. The record path gains one shift; each histogram grows to 8 KiB and
  moves behind a `Box` so a `Store` does not carry 80 KiB inline. `_ns_p95` is
  emitted alongside `_ns_p50`/`_ns_p99`, since p95 is the figure the README and
  the dashboard both quote.

- **`GET /v1/annotations` no longer requires `trace_id`**, and gained
  `source` (prefix match), `since`, `until` and `limit`. Scores are produced
  per trace but are only meaningful as a population; requiring a trace meant an
  eval run could be read only one trace at a time, which is to say not at all.

- **`GET /v1/spans` returns an envelope**, `{spans, next_cursor, cost}`, rather
  than a bare array — the carrier for the two items above.

- **The dashboard is rebuilt: seventeen screens on a left rail** grouped by the
  question you arrived with, replacing four tabs over five views. New: Overview,
  Latency, Failures, Scores, Experiments, Datasets, Live tail, Trace compare,
  Server, Connect. Rebuilt: Traces (predicate builder reaching the whole
  parameter surface, drag-to-zoom volume brush, sortable columns, cursor
  paging, query cost) and Trace detail (time ruler, minimap, drag zoom,
  subtree collapse, critical path, self time, agent mode).

  A query is now a value that serializes into the hash route, so the search
  that found the bug is a link you can send; `⌘K` opens a palette that takes a
  pasted trace id, which previously had no front door at all. Reads carry an
  `AbortSignal` so a superseded query stops occupying a connection, identical
  in-flight GETs are coalesced, and polling stops in a background tab.

### Fixed

- **A store under sustained ingest was never compacted.** A merge declined
  outright when a seal held a claimed-but-unpublished segment id, rather than
  waiting for it. The guard is a correctness requirement — that seal publishes
  newer data under a lower id, and merged output taking a higher one would
  outrank it, inverting last-write-wins — but declining made compaction
  hostage to the write rate. A seal is in flight for much of the time under
  load, so a tick that checked once and gave up almost never found the store
  quiet: measured at 25,000 spans/s, **one tick in sixteen achieved anything**
  and the segment count grew without bound (14 to 215 in six seconds) while
  compaction ran on schedule and did nothing. It recovered the instant writes
  paused, which is why the effect is invisible on an idle store and total on a
  busy one — the case compaction exists for.

  A merge now takes the seal permit instead of testing a counter, **held**
  only long enough to choose a run and claim its output ids. Acquiring it is
  the slow half: a seal owns the permit from drain through write, fsync,
  rename, reopen and reconcile, so a merge arriving mid-seal waits out all of
  that — bounded by one seal, and on the maintenance thread. Expiry already
  takes the same permit and holds it for a whole deletion.

  Most ingest is unaffected, because a seal that cannot take the permit
  coalesces into the next one. Two paths wait rather than coalesce:
  `Durability::Flushed`, which must seal before it acknowledges, and any mode
  once the buffer reaches four times `flush_spans`, where waiting is the
  backpressure. Both wait only on the microseconds a merge holds the permit,
  never on the merge itself.

  Under the same load every tick now compacts, and the segment count stays
  bounded — oscillating between 15 and 80 over six seconds, against 14 climbing
  to 215 before. The `unpublished_seals` counter is gone; the permit is what it
  was approximating.

  Choosing the run moved inside those guards too. Scanning first and then
  re-checking under the lock left a window for a seal to land and make the
  answer stale, which retired the whole tick.

  And a tick is now bounded to the backlog it **found**: segments sealed after
  it started are left for the next one. Without that, merging kept pace with
  arrivals and a single call ran until the writes stopped — 2,213 segments
  merged away and still going.
- **The index-memory benchmark's "peak RSS" was not a peak.** It took one RSS
  sample after `compact_segments()` returned — after the merge has freed its
  working set — so it measured the trough and the capacity guide published it
  as the peak. A comment claimed a peak sampler existed; none did. RSS is now
  sampled every 20 ms by a background thread for the duration of the merge,
  and the real figures are far higher: a store serving in 10 MiB peaks at
  **1,204 MiB** to compact itself, not 721. Every completed merge in the
  record peaks between 1,204 and 1,601 MiB, taken at load average
  36-45 and varying up to 8% between runs of the same matrix — the order of
  magnitude is the finding, not the individual figures.
- **A failed probe was published as a zero-memory result.** A child that died
  produced an empty JSON object, and the reporter turned missing fields into
  `0.0` — so the most important failure this benchmark can have, the child
  being OOM-killed on the very configuration whose memory is in question,
  rendered as `0.0 MiB`: the best-looking number in the table. Probe failure
  and unparseable output now abort, naming the configuration and the child's
  stderr. Verified by simulating an OOM kill.
- **Compaction rows were reported for runs that never compacted.** 21 of 26
  configurations return 0 from `compact_segments()`, and their steady-state
  RSS was appearing under a compaction heading. They now read "did not merge".
- Per-key index diagnostics moved to their own `--by-key` invocation: computing
  them reopens and decodes every segment, and the allocator held those freed
  blocks across the RSS reading that followed.
- The capacity guide claimed "none is estimated" while carrying an explicitly
  extrapolated 29 GB projection. It now states the exception and cites the
  measurement records by name.
- **Compaction stopped permanently at the first segment that did not match its
  neighbours' size tier.** `tail_run_to_merge` took the tier of the *last*
  segment and walked back only while the tier matched, so any discontinuity at
  the tail cut the run to length 1 — below `fanout` — and nothing merged. Two
  ways that happened in practice, both measured. Ingest seals when the write
  buffer reaches `flush_spans` and batches do not divide evenly, so a finished
  load ends on a partial segment; below `base_bytes` it is a tier of its own,
  and 10M spans at the defaults compacted **0 of 977 segments**. Separately, a
  merge stopped by `max_segment_bytes` left a cap-sized segment at the tail,
  one tier *above* the segments behind it, which froze those the same way: 20
  equal segments went to 17 and then stuck. Neither self-healed. Nothing
  behind the tail is ever a merge candidate again, so continued ingest
  compacted only the region after the wall and left the prefix — hundreds or
  thousands of segments — permanently unmerged. Filtered search costs one
  index narrowing per segment, so this is the exact cost compaction exists to
  bound.

  Two changes. **Only a larger segment ends a run** — the tier that matters is
  the largest in the run, and a smaller neighbour rides along as a passenger
  rather than blocking it (passengers never count toward `fanout`, so four
  tiny segments still cannot justify merging into a 256 MiB one). And **a run
  now merges into a group of outputs rather than one**, splitting at
  `max_segment_bytes` instead of truncating the run to fit, so the cap bounds
  each output without stranding the remainder. Outputs take a contiguous block
  of ids in group order, which is what keeps last-write-wins intact across
  them; dedup stays within a group, so a merge still holds only one output's
  spans in memory. On the shapes above: 977 segments now compact to the cap
  floor instead of 0, and a store compacting as it ingests settled at 10
  segments where it previously drifted to 30.

  The compaction journal is now one record per merge —
  `.supersede.<first-output>.journal`, naming every input and every output —
  because which way to finish an interrupted merge is a fact about the group.
  Recovery deletes the inputs once every output is present and parses, and
  otherwise **rolls the merge back** — a lone output carries a higher id than
  every input while holding only its own group's view of a key, so left beside
  intact inputs it would shadow a newer version in a group whose output never
  landed. Rollback requires *every* input still to be present: any one already
  gone proves deletion had started, which happens only after every output was
  durable, so a missing output is one a later merge has since consumed and
  recovery must roll forward instead. A journal that saw one input at a time
  could not tell those apart and would delete live segments holding the only
  copy of an input already removed.

## [0.19.0] - 2026-07-25

**Segment sealing no longer holds the writer lock**, which was 74% of
everything that lock was held for and 88% of a run's wall clock. Ingest rises
37-116% depending on `--flush-spans`, and `--flush-spans` stops being a
throughput setting at all.

Alongside it, four defects from an independent review of v0.17, fixed and
tested. They are symptoms of one gap — Traza has several recovery domains and
nothing names a state they all agree on — so the class fix is designed too, and
scheduled before 1.0 rather than as part of HA:
`docs/generations-design.md` (since moved out of the repository).

### Fixed

- **TTL-expired spans came back after a restart.** Expiry removed them from the
  write buffer and left the write-ahead log records that carried them intact,
  so recovery replayed the expired span and `record_count` went back up.
  Expiry now rewrites the log to exactly the spans that survived — staged and
  renamed, because the survivors are still acknowledged and must not be lost to
  a crash mid-rewrite. The expired bytes leave the disk in that pass rather than
  being marked dead, which is also what "deleted on request" has to mean.
- **Damage in the middle of the write-ahead log silently dropped acknowledged
  batches.** Replay stopped at the first length, CRC or JSON failure and
  returned everything before it as if that were the whole log: three fsynced
  frames with a corrupt second one recovered as one, and the third vanished
  without a word. Recovery now distinguishes the two cases. A frame missing
  bytes it declared can only be the interrupted final append — it is dropped,
  and the file is truncated back to the last complete frame so those bytes
  cannot become interior bytes after the next append. A frame that is complete
  but fails its checksum or decode fails the open, naming the byte offset and
  what moving the log aside would cost. See
  [when the log will not open](docs/operations/durability.md#when-the-log-will-not-open).
- **A concurrent export was not one dataset.** `GET /v1/export` ran an
  independent query per 4,096-row page, so a span re-ingested behind the cursor
  was emitted twice — 5,001 rows and two versions of one primary key, under
  `X-Traza-Export-Complete: true`. Export now pins a `SnapshotView` and pages
  that: the write buffer is copied and the segment set is reference-counted, so
  compaction and expiry may unlink files the export is still reading and the
  space returns when it finishes. `complete: true` now means "this is the whole
  dataset as of the first byte", each primary key appearing at most once.
- **Hot-key updates grew the log without bound.** The automatic flush threshold
  counted unique buffered records, which an update to an existing key never
  advances: with `--flush-spans 2`, 500 acknowledged updates to one key left
  `buffered_records: 1`, `segment_count: 0` and a log of 108 KB that would never
  seal. `--flush-spans` now applies to upserts since the last seal as well as to
  unique records, and a new `--flush-wal-bytes` (default 64 MiB) bounds the log
  directly. Recovery also streams frames instead of reading the whole log into
  memory.
- **A failed expiry was not retryable, and resurrected spans.** Expiry mutated
  memory before the durable change it corresponded to: it dropped the span from
  the write buffer before the log rewrite succeeded, and removed a fully
  expired segment from the live list before unlinking its file. Either failure
  left memory ahead of the recovery authority — and left nothing for the retry
  to find, so the next pass reported `Ok(0)`, never repaired the log or the
  file, and the restart brought the data back. Both now change durable state
  first and memory second, with the in-memory step infallible, so a failed
  expiry leaves the store exactly as retryable as it found it. `Wal::rewrite`
  additionally moves every fallible step before its rename, so its failure is
  never ambiguous about which log is live.
- **A deleted segment file was reported gone before the deletion was durable.**
  An unlink is visible immediately but survives a crash only once the directory
  entry it removed is synced, so expiry could report a segment deleted — and
  drop it from the live list — over a file a crash would bring back, spans and
  all. Expiry and compaction now sync the directory after unlinking and before
  anything downstream depends on it, and the unlink is idempotent so a retry
  after a partial one can finish instead of failing forever on `NotFound`.
- **Retention rewrote segments under a new id**, which moved them to the newest
  position in an order that *is* recency order — so after a partial expiry a
  re-ingested span could revert to an older version held by the rewritten
  segment. The survivors are renamed onto the same name now, keeping the
  segment's place. Found while fixing the above.

### Changed

- **Segment sealing no longer holds the writer lock.** It was the largest thing
  the engine did while holding the lock every ingesting thread needs:
  converting spans to records, encoding the segment, writing it, fsyncing it,
  renaming it, fsyncing the directory and reopening the result — all on a
  private vector no other thread can reach. Measured at concurrency 8 before
  the change, the lock was held 88% of a run at the default `--flush-spans`
  and **74% of that was the seal**; at `--flush-spans 5000` it was 97% and 81%.
  A seal now drains the buffer under a short lock, does every byte of I/O with
  nothing held, and publishes under a short lock — the shape compaction and
  retention already had.

  Before and after with the two builds alternated round-robin on a contended
  host, median of four rounds at concurrency 8: `--profile throughput`
  162,763 → **222,683** spans/s (+37%), `balanced` 116,612 → **176,004**
  (+51%), `latency` 83,400 → **180,331** (+116%). Those levels are depressed
  by background load; the round-robin is what makes the ratios trustworthy.
  **`--flush-spans` has stopped being a throughput knob** — `latency` and
  `balanced` now land within 3% of each other, where they used to span 2x — so
  set it for the tail latency and buffer memory you want.

  Two consequences worth knowing:
  - **The write buffer can exceed `--flush-spans`** while a seal is in flight,
    because ingest no longer waits for one. Past four times the threshold an
    ingesting thread waits for the seal to publish, so it stays bounded — but
    size memory for that bound.
  - **`--flush-wal-bytes` now governs the log's real size** under sustained
    ingest. A seal that empties the buffer still discards the whole log, so a
    quiet store is unchanged; a busy one lets the log run to that bound between
    reclamations rather than emptying it on every seal, because rewriting the
    log to the survivors every time would put thousands of re-serialized spans
    straight back under the writer lock. Restart replay is bounded by the
    setting, which is what it always documented.

  What made this safe rather than fast-and-wrong: the drain **copies** the
  buffer instead of emptying it, so already-acknowledged spans are never in
  neither the buffer nor a segment — a merge keeps its inputs live until its
  output is published, and a seal now does the same. The buffer holds
  `Arc<Span>` so that copy is pointer-sized and so the post-publish eviction
  can ask *is the value under this key still the one I sealed* by handle
  identity. Comparing values would have destroyed data: a span re-ingested
  unchanged during a seal is a newer version that happens to look identical.
  `tests/seal_concurrency.rs` races reads, ingest and expiry against a seal;
  `tests/durability.rs` adds a SIGKILL taken mid-seal.
- **`traza_segment_seal_locked_ns_*` and `traza_segment_seals_coalesced_total`**
  are new on `/v1/metrics`. The first is the part of a seal that holds an
  engine lock; against `traza_segment_seal` it is the only way to see that the
  write is off the lock, because query results are identical either way. The
  second counts seals that found another already in flight and declined to
  start a second one.
- **Compaction and retention no longer stop the server.** Both held the segment
  lock across parsing every input, materializing the result and fsyncing it;
  queries waited on that lock while holding the writer lock, so ingest queued
  behind the queries. A merge measured in gigabytes was an outage measured in
  gigabytes. Both now pin their inputs, do every byte of I/O with no engine lock
  held, and take the lock back only to publish — after re-checking that what
  they pinned is still there. A new maintenance lock serializes the two against
  each other, and only against each other. `tests/compaction.rs` measures it:
  the slowest read or ingest during a merge must be a fraction of the merge.
  What this buys is that reads and ingest no longer *wait* on maintenance; they
  still share CPU and disk with it, and that contention remains unmeasured.
- **`Store::snapshot`** is public API, returning a `SnapshotView` that answers
  from one pinned instant however the store changes afterwards. Any multi-step
  read should use it; a lock cannot span pages.
- **`Error::WalCorrupt`** is a new variant, for the refusal above.
- **`Config::flush_wal_bytes`** is a new field (`Some(64 MiB)` by default). A
  `Config` built by struct literal needs it; `..Config::default()` does not.
  Documented in the library `Config` table alongside the server flag.
- **The generations design carries the log inside the boundary.** `CURRENT` and
  a global `wal.log` are two recovery authorities that no rename can publish
  together, so the design now stamps every frame with the generation epoch it
  belongs to, records `folded_through` in the manifest, and replays only frames
  after it. Publishing `CURRENT` is staged, renamed and **directory-fsynced
  before a single folded frame is reclaimed** — a rename is not crash-durable
  until then, and a durable log reclamation against a `CURRENT` that rolls back
  is the one combination that loses acknowledged writes. The crash matrix
  covers both sides of that fsync, and reclaiming folded frames is described as
  the roll-over it has to be: they are a prefix, and truncation only removes a
  suffix.

## [0.18.0] - 2026-07-25

Search stops answering real questions with silence, and gains the predicates
its own analytics already implied.

Search gains the predicates its analytics already implied, and stops
answering two classes of question with a silent empty result.

### Added

- **Range, negation and ordering predicates**: `min_attr.KEY` / `max_attr.KEY`
  (numeric, reading stringified numbers too), `not_attr.KEY`,
  `max_duration_ms` / `max_duration_ns`, and `sort=duration|-duration|start|-start`.
  Token and cost analytics could already aggregate what search could not find,
  so "which calls cost more than a cent" and "the ten slowest" were
  unanswerable. `not_attr.KEY` keeps spans that lack the key entirely — "not
  known to be an error" includes spans that never recorded a status.
- **Segment timestamp ranges (format v3)**, letting a time-filtered query skip
  a segment's records. (Every segment is opened and its indexes parsed at
  store startup; pruning avoids the record reads, not the open.)
  `since`/`until` were pure post-filters, so a
  "last 15 minutes" search read every segment in the store. v2 segments are
  still read; they carry no range, are never skipped, and age out through
  compaction.

  **No latency improvement is claimed for this yet.** Pruning is verified to
  skip the right segments by counter, but an attempt to measure the payoff
  produced a *negative* result (a windowed query slower than an unwindowed
  one) on a 40-segment store under load. Forty segments is far too few for
  per-segment probe cost to dominate — the compaction work needed thousands
  before the effect was visible — so the benchmark was measuring noise. The
  mechanism is sound and the work avoided is real; the latency benefit is
  unmeasured, and is recorded as unmeasured rather than assumed.
- **`traza_segments_pruned_by_time_total`** and
  **`traza_segments_examined_total`** in `/v1/metrics`. Pruning is invisible
  from results — a skipped segment and a scanned one give the same answer — so
  these are the only way to see it working.

### Changed

- **Attribute filters match scalars by value, not by type.** `attr.code=200`
  now finds spans that stored `200` and spans that stored `"200"`. Previously
  only the JSON reading matched, so a store of stringified codes answered
  every such query with an empty array indistinguishable from no-such-data.
  Containers still compare structurally.
- **The index probe is chosen by selectivity rather than by a fixed order.**
  Only one predicate can drive a scan; the planner took `service`, then
  `name`, then whichever attribute came first. `service` is usually the least
  selective term in a trace store, so adding a precise attribute filter to a
  service query made it *slower* — it read every span of the service and
  discarded almost all of them. The smallest posting list now wins.

### Fixed

- A sorted query ranks **every** match rather than the first page. Sorting
  cannot stream, so past an internal candidate ceiling the query is refused
  with `400` and guidance to narrow it — a "ten slowest" computed over an
  arbitrary first page is a wrong answer that looks like a right one.

## [0.17.0] - 2026-07-25

Ingest throughput roughly doubles at concurrency, and the record of why is
corrected in three places where it was wrong. Persistent connections, OTLP
decoded straight to spans for both wire formats, `--profile` for the
throughput/latency tradeoff, and a documentation set for users, operators and
developers.

Ingest throughput: 108,881 -> 208,973 spans/s at 16 concurrent clients in
`wal` mode, measured through one client against both builds. The roadmap's
250k target is still 16% away, and the benchmark now says exactly why.

### Added

- **Persistent HTTP connections.** Every response used to carry
  `Connection: close`, so a client paid a connect and teardown per batch.
  Keep-alive is now the default for HTTP/1.1. Worth +11% at batch=20 and
  nothing at batch=1000 — the honest number, not the hoped-for one.
- **`GET /v1/metrics`** in Prometheus text format: per-stage ingest timings
  (writer-lock wait, WAL encode/write/fsync, buffer upsert, segment seal,
  decode), request latency, and connection counters. Stage percentiles are
  power-of-two bucket bounds and are documented as approximate; they exist to
  rank stages, not to be published as latencies.
- **`--max-connections`** (default 1024), replacing `--workers`. Keep-alive
  means a connection occupies its handler until the client is done, so a fixed
  worker pool would serve N clients and leave the rest queued indefinitely.
  Past the limit clients get `503` rather than silence.
- **`--wal-commit-window-us`** (default off): holds an fsync open briefly so
  more batches join it. A latency-for-amortization trade that does not touch
  the guarantee — the fsync still precedes the acknowledgement it covers.
- **`--profile throughput|balanced|latency`**, setting the write-path knobs
  (`--flush-spans`, `--wal-commit-window-us`) as a coherent group so they can
  be chosen by intent rather than by reading the internals. An explicit flag
  always beats the profile, in either argument order. **No profile can change
  `--durability`** — a profile cannot represent one, so none can silently make
  writes lossy. Measured tradeoffs, including where each profile does *not*
  help, are in [docs/configuration.md](docs/configuration.md).
- **A documentation set for three audiences** under `docs/`: a user guide
  (getting started, data model, ingest, full HTTP API reference, trace
  browser), operations (deployment, durability, administration, monitoring,
  capacity), and internals (architecture, the load-bearing invariants, module
  map, testing, benchmarking). The README is now an overview that routes
  onward rather than holding all of it.
- **`ingest-bench`**, a benchmark matrix over protocol, keep-alive,
  concurrency and durability. Reports the median of N runs with its spread,
  refuses to report a rate from a run that shed a connection or stored fewer
  spans than it acknowledged, and restarts the server to re-verify every
  non-`buffered` result. `TRAZA_BENCH_SERVER` points it at another build so
  before/after runs share one client.

### Changed

- **`POST /v1/spans` decodes straight to `Vec<Span>`.** It used to parse to a
  `serde_json::Value`, deep-clone the array out of it, then re-walk that DOM
  once per span — three passes and three sets of allocations for one job.
- **`POST /v1/traces` decodes straight to `Vec<Span>` too, for both
  encodings.** Protobuf lowered to the OTLP/JSON `Value` shape and OTLP JSON
  parsed into the same shape, and both then re-walked that DOM. Protobuf
  additionally hex-encoded every trace and span id through a `format!` per
  BYTE. Decode is now **9.2x cheaper for protobuf** (4,384 → 479 ns/span) and
  **1.9x cheaper for OTLP JSON** (2,377 → 1,275 ns/span), medians of 5 runs of
  1M spans at concurrency 1. Decode is ~2% of ingest cost, so this is a CPU
  and correctness result, not a throughput one. The mapping rules the two
  decoders must agree on are shared rather than duplicated, and a differential
  test pins that agreement across every `AnyValue` variant.
- **`ingest-bench` measures latency, not just throughput**, with an open-loop
  fixed-arrival-rate mode and coordinated-omission correction. Under a
  closed-loop generator every saturating configuration reports latency that is
  just concurrency over throughput, so the tradeoff a latency profile exists
  to make was not visible at all. Scenarios also run round-robin with their
  order rotated per round, so background load hits every configuration alike
  instead of landing on whichever ran during a spike.
- **`ingest-bench` separates wire format from route.** It posted JSON to
  `/v1/spans` and protobuf to `/v1/traces`, so every protocol comparison also
  contained the OTLP mapping; on that basis this project claimed protobuf was
  slower than JSON. A third protocol, `otlp-json`, holds the route fixed, and
  scenario labels now name the route. Measured properly, **protobuf decodes
  2.3–2.7x faster than OTLP JSON** on payloads 2.9x smaller. The benchmark
  also reports bytes/span and decode ns/span per scenario.
- **The WAL encodes a batch before taking the writer lock.** Serializing under
  the lock made every concurrent ingest wait on one thread's JSON encoding.
  Only the file write remains inside it.
- **Sealing a segment no longer clones the write buffer**, and puts the spans
  back if the write fails.
- Request framing is stricter now that connections persist: transfer-encoded
  bodies and duplicate `Content-Length` headers are refused rather than
  resolved, because either ambiguity lets one request be split in two with the
  remainder attributed to the client's next request.

### Fixed

- **`bytesValue` attributes are no longer dropped.** An OTLP attribute of the
  bytes variant was stored as `null` on both ingest paths, with no error and no
  warning. It is now stored as lowercase hex, the same representation trace and
  span ids use — protobuf's raw bytes and OTLP/HTTP JSON's base64 land on the
  one value, so what an attribute holds does not depend on how it arrived.
- A connection refused at the limit now reliably receives its `503`. Closing a
  socket while the client's request bytes sat unread made the kernel send RST,
  and the RST beat the response — backpressure surfaced as "connection reset
  by peer".

## [0.16.0] - 2026-07-24

Search that scales with the store: size-tiered compaction bounds the segment
count, and at a 1 GiB segment cap filtered-search p99 clears the project's own
50 ms bar at 100M spans for the first time.

### Added

- **Size-tiered compaction**, on by default and configurable with
  `--compaction-fanout` (0 disables) and `--compaction-max-segment-bytes`.
  Filtered search costs one index probe per segment, so a store that only
  appends flush-sized segments gets steadily slower to search as it grows.
  Compaction merges same-size segments to bound that count.
  - Measured over 10M spans, uncompacted vs default compaction: attribute
    filter p50 14.8 -> 2.4 ms, p95 33.4 -> 4.1 ms, p99 220 -> 14.1 ms
    (6-15x), and trace lookup p99 4.65 -> 2.28 ms. It costs ingest
    throughput, which is the trade the flag exists to let you make.
  - Measured at **100M spans** (55 GB on disk), uncompacted vs default,
    both through the same harness: attribute filter p50 155.5 -> 9.8 ms,
    p95 747.3 -> 27.1 ms, p99 1664.6 -> 72.9 ms (16-28x), trace lookup p99
    7.72 -> 1.82 ms, segments ~10,100 -> ~380. It costs about 31% of ingest
    throughput (59,025 -> 40,894 spans/s) and ~1.5 GB of resident memory for
    the merge working set.
  - With the 256 MiB default cap **the filtered-search target is missed at
    that size**: p99 72.9 ms against the project's own 50 ms bar (p50 and
    p95 are inside it). Raising `--compaction-max-segment-bytes` to 1 GiB
    clears it — **p99 22.2 ms, p95 9.3 ms, p50 2.3 ms**, with trace lookup
    p99 0.99 ms, measured on the same corpus through the same harness. The
    binding constraint was the cap, not the algorithm: it floors segment
    count near corpus/cap, and the sampled count fell from ~380 to ~100-125.
  - That cap is a real trade, not a free win. Peak RSS rises 2.0 -> 6.7 GB
    (a merge materializes its inputs, so the working set tracks the cap) and
    sustained ingest falls a further 24%, 40,894 -> 31,267 spans/s. Measured
    at 100M on one machine in a single run; untested above that size.
  - Uncompacted, every segment holds an open file descriptor — ~10,100 at
    100M, which would exhaust a default 1024-fd limit. A second reason not
    to disable compaction on large stores.
  - Only the TAIL of the segment list is merged. Segment path order IS
    recency order, and a merged segment takes a fresh (newest) id, so
    merging a run from the middle would promote its spans past segments that
    legitimately supersede them.
  - Crash safety reuses the existing supersede journal with one marker per
    input, and the merged segment is renamed into place before any input is
    deleted.
  - TTL expiry keeps its one-minute cadence; compaction runs on a 5 s tick in
    the same maintenance thread, which now also starts when only compaction
    is enabled.

### Changed

- The benchmark can vary the segment size cap:
  `TRAZA_BENCH_COMPACTION_MAX_SEGMENT_BYTES` passes
  `--compaction-max-segment-bytes` to the server it spawns, parallel to the
  existing `TRAZA_BENCH_COMPACTION_FANOUT`. It defaults to the real
  `CompactionConfig::default()` value rather than a literal, so the two
  cannot drift, and the configuration is named in the generated
  `BENCHMARKS.md` row. Without this the cap could not be measured at all.
- `Config` gains a `compaction` field (`None` disables).
- README performance claims corrected against a measured 10,000,000-span run.
  The previous text dated from 0.3.1 and claimed search was "effectively
  scale-independent" at p50 2.9 ms; the measured filter p50 was 14.8 ms
  across ~1000 segments. The README now states that filtered search scales
  with segment count, and reports measured RSS (0.25 GB, not 0.71 GB) and
  disk (~6 GB for the benchmark's span shape, not 2.4 GB).
- Renamed the segment module `segment_v2` → `segment` (file, module,
  `segment_error` helper, acceptance test, format doc): there is only one
  segment format, so the version in the name was redundant. The suffix
  constants flipped too — the current format is now the unmarked
  `SEGMENT_SUFFIX` (`.seg`) and the unsupported legacy JSONL suffix is
  `LEGACY_SEGMENT_SUFFIX`.
- `tests/segment_format_acceptance.rs` now asserts against the real encoder's
  output instead of a fixture it invented. The old fixture described itself as
  "deliberately independent" but was only ever checked against itself, never
  fed to the reader — so it drifted into a layout Traza has never written (a
  u32 header length at offset 12, three sections instead of four, a `.trz2`
  extension) and passed continuously while asserting nothing. Feeding it to
  `Segment::from_bytes` failed with `Corrupt("invalid v2 header length")`.
  The tests still parse the header by hand at fixed offsets — that
  independence is the point — but they parse bytes from
  `segment::encode`, round-trip them through `Segment::from_bytes` and
  `Segment::open`, and pin the four-section layout, the record encoding, and
  what the reader must reject. `reopen_persistence` no longer writes bytes
  and reads them back asserting equality (a test of `fs`, not of Traza).
- `docs/segment-format.md` describes the layout that shipped. It previously
  documented only the pre-implementation proposal — a trailing footer, a
  JSONL payload, and readable v1 segments — none of which match
  `src/segment.rs`; v1 JSONL is rejected with a migration pointer. The
  proposal is retained below the real layout, marked as history.
- Reconciled the on-disk segment magic. The encoder wrote `TRAZAV2` while the
  acceptance test and format doc expected `TRAZASEG`; they never met, because
  the acceptance fixture is self-checked and was never fed to the real reader.
  The magic is now `TRAZASEG` (the version lives in the `VERSION` field, not
  the magic), matching the docs, and the acceptance test pins it to the real
  `segment::MAGIC` constant so the two cannot drift again. **On-disk format
  change:** existing `.seg` files written before this are no longer read
  (acceptable pre-release; no data migration is provided).

## [0.15.0] - 2026-07-24

Durability: an acknowledged write now means what the deployment says it means.

### Added

- **Write-ahead log with group commit.** Ingest appends a batch to `wal.log`
  and fsyncs it BEFORE acknowledging; the log is replayed into the write
  buffer on open and reclaimed once a flush seals those spans into a segment.
  The fsync runs outside the writer lock, so concurrent batches coalesce into
  one sync instead of serializing an fsync per request — measured 13.7k
  spans/s at concurrency 1 rising to 48.1k at 16, where per-batch fsync would
  stay flat.
- **Explicit durability modes**, selected with `--durability` and reported in
  every ingest response and in `/v1/stats`, so a client never has to infer
  what a `200` promised:
  - `buffered` — accepted in memory; lossy by design, and no longer the
    default.
  - `wal` (**new default**) — fsynced to the log and recovered on restart.
  - `flushed` — present in a sealed segment.
- `/v1/stats` reports `durability` and `wal_bytes` (the work a restart would
  replay).
- `tests/durability.rs` holds each mode to its own claim under **SIGKILL**:
  `wal` and `flushed` lose nothing acknowledged, `buffered` is verified to be
  lossy rather than accidentally durable, recovery preserves last-write-wins
  for a re-ingested span, the log is reclaimed after a flush without losing
  data, and 800 spans acknowledged across 8 concurrent writers all survive.

### Changed

- `Config::default()` is now `Durability::Wal`. A store that silently loses
  acknowledged writes is the wrong default even though it is the faster one.
  `Config` gains a `durability` field, so code constructing it as a struct
  literal must add one (or spread `..Config::default()`).
- The benchmark measures the default (`wal`) and labels the mode, rather than
  reporting a `buffered` number no production deployment could rely on.
- `tests/auth.rs` no longer pins socket teardown to `ECONNRESET`; the
  invariant is that a complete HTTP response arrived, and pinning the errno
  made the test flaky under parallel load.

### Notes

- `fsync` on macOS does not flush the drive's write cache (`F_FULLFSYNC`
  would, and std does not expose it), so a macOS power cut can still lose an
  acknowledged write. Process death cannot, on any platform. Documented in
  the README and `src/wal.rs` rather than left implied.

## [0.14.0] - 2026-07-24

OpenLLMetry-native tracing, a standalone dashboard served from its build
output, and a trace browser that renders what agents actually produce.

### Added

- Traza now follows the [OpenLLMetry](https://github.com/traceloop/openllmetry)
  standard (Traceloop's OpenTelemetry GenAI conventions). Sessions, token
  analytics, and the dashboard recognize the current OTel GenAI attributes —
  `gen_ai.provider.name`, `gen_ai.operation.name`, `gen_ai.usage.input_tokens` /
  `output_tokens`, `gen_ai.request/response.model`, `gen_ai.conversation.id`,
  and `traceloop.*` — so an OpenLLMetry-instrumented app populates every
  derived view over OTLP with no attribute renaming. The OTel-deprecated names
  (`gen_ai.system`, `gen_ai.usage.prompt_tokens` / `completion_tokens`) and
  Traza's native `llm.*` / `session.id` shorthand are accepted as aliases;
  native behavior is unchanged. A new `src/semconv.rs` normalization layer is
  the single source of truth for the key precedence.
- `GET /v1/stats/llm?group_by=provider` rolls tokens up by the resolved
  provider.
- `GET /v1/spans?session=<id>` filters spans to a session, unioning every
  recognized session key so a session whose spans mix conventions (some
  `session.id`, some `gen_ai.conversation.id`) is returned whole. Sessions are
  grouped by any recognized key, and each reports the `session_attribute` that
  grouped it.
- The dashboard's span detail renders provider/model/token chips and a Messages
  panel from the current JSON `gen_ai.input.messages` / `gen_ai.output.messages`
  as well as the legacy indexed `gen_ai.prompt.*` / `gen_ai.completion.*`
  attributes and native `llm.prompt` / `llm.completion` events.

### Changed

- The dashboard is no longer compiled into `traza-server`. The server now
  serves the UI's build output from disk: `--ui-dir` (default `./ui/dist`,
  produced by `cd ui && npm run build`) backs `GET /` and `GET /dashboard`.
  Building the server still needs no Node toolchain, a rebuilt UI is picked up
  without restarting, and a missing build is not fatal — the API runs and the
  UI routes 404 with build instructions. The shell stays served before the auth
  gate (it is static build output carrying no data) while every `/v1` call it
  makes remains gated. Path traversal out of the UI directory is refused.


- License is now Apache-2.0 only (previously dual MIT OR Apache-2.0).
  `LICENSE-MIT` is removed and `LICENSE-APACHE` renamed to `LICENSE`.

- `ci.sh` is the merge bar for the whole tree: it now builds and tests the
  dashboard (`npm ci`, `npm test`, `npm run build`, Node per `ui/.nvmrc`) and
  rejects source files containing a NUL byte. Rust tooling cannot police the
  UI, and a broken Vite build must not merge green. `TRAZA_SKIP_UI=1` runs the
  Rust half alone.
- The dashboard has unit tests (`ui/`, vitest): message parsing, content-type
  detection, the markdown subset, and the syntax tokenizer — including that
  highlighting round-trips to the exact source and never linkifies a
  `javascript:` URL.

### Fixed

- Session resolution now resolves every recognized key under ONE snapshot of
  the write buffer and segment list. Querying each key separately let a span
  re-ingested between the queries be seen first in its superseded version,
  which then locked the newer version out — breaking last-write-wins during
  ordinary concurrent ingest.
- A numeric session id (`"gen_ai.conversation.id": 4711`) can now be opened,
  not just listed. Normalization stringifies numeric attributes, but the
  lookup matched only JSON strings, so such a session appeared in
  `/v1/sessions` while `/v1/sessions/4711` returned 404 and
  `/v1/spans?session=4711` returned nothing.
- The server finds a packaged dashboard: with no `--ui-dir` it searches
  `$TRAZA_UI_DIR`, `<binary dir>/ui`, `<binary dir>/../share/traza/ui`, then
  `./ui/dist`, and lists every path it tried when none has a build. A
  CWD-relative default alone meant an installed binary served nothing unless
  it was launched from a checkout.
- The conversation view pages through long sessions and says when it is
  showing a prefix. Spans come back oldest-first, so a fixed cap silently
  dropped the newest turns while presenting the result as complete.
- `ui/src/views/ConversationView.jsx` no longer contains a literal NUL byte,
  which made git treat the file as binary and hid it from diff and blame.

### Removed

- The checked-in generated `src/dashboard.html` and the `ui/scripts/embed.mjs`
  script that produced it, along with `src/dashboard.rs`. UI builds no longer
  regenerate an embedded HTML file, so `ui/` changes no longer produce a
  368 KB diff in the Rust crate.

### Notes

- Cost analytics remain a Traza extension (`llm.cost_usd`), not part of
  OpenLLMetry — OpenTelemetry GenAI defines no cost attribute. Cost populates
  only when the ingest pipeline supplies it.

## [0.13.0] - 2026-07-23

Wire-contract release: `/v1/stats` renames its counters to record
terminology and `/v1/export` switches to chunked framing with
completion trailers — clients parsing either surface must update.

### Fixed

- Export pagination now uses the engine's exclusive full-key
  `(start_time, end_time, trace_id, span_id)` cursor with a fixed 4,096-row
  page. Equal-timestamp runs no longer trigger exponential prefix re-fetches
  or corpus-sized pages, and bounded queries borrow resident posting lists
  instead of cloning them per page.
- Export responses use HTTP chunked framing with explicit
  `X-Traza-Export-Complete` and `X-Traza-Export-Count` trailers. A storage or
  serialization failure after `200 OK` can no longer masquerade as a complete
  dataset.
- Annotation replay now tolerates only an unterminated final append. A
  malformed newline-terminated middle record fails startup instead of
  silently hiding every valid annotation after it; a torn tail is truncated
  before new appends, a missing final delimiter is restored, and annotation
  creation/rewrite renames also fsync the parent directory.
- LLM/session integer counters saturate instead of panicking in debug builds
  or wrapping in release builds. Non-finite cost strings are ignored and
  floating sums remain finite.
- Payload sweeping holds the touch-registry lock only across each final
  eligibility check and deletion. The ingest race remains excluded without a
  large directory walk stalling every oversized-payload write.

### Changed

- `/v1/stats` and `Store::stats` now name their cheap physical storage counts
  as `record_count` / `*_records`. Immutable historical versions remain
  physical records until compaction even though last-write-wins queries expose
  one logical span.
- The server now binds `127.0.0.1` by default. Unauthenticated non-loopback
  binds are refused unless the operator configures `TRAZA_TOKENS` or passes
  `--allow-unauthenticated-non-loopback` explicitly.
- Current documentation now matches the v2-only file-backed engine,
  OTLP/HTTP protobuf support, v0.12 crate line, export integrity contract, and
  safe bind defaults.

## [0.12.2] - 2026-07-23

### Fixed

- **Payload TTL race** (found in review): compaction snapshotted live
  references, released the locks, then swept — an ingest committing a
  new reference to an old deduped file inside that window authorized
  its deletion. The store is single-process (DirectoryLock), so an
  in-memory touch registry now records every payload write/dedup
  BEFORE filesystem work; the sweep spares anything touched within a
  10-minute immunity window in addition to the live-reference set.
- **Concurrent identical-payload ingest** (found in review, reproduced
  as 9 successes + one ENOENT): all writers shared one `<hash>.tmp`
  path, truncating each other's temp and racing the rename. Temps are
  now writer-unique; every rename is valid, and identical content
  makes the last rename byte-identical anyway.
- **Export truly streams** (found in review): `GET /v1/export`
  materialized the complete query result plus a complete NDJSON
  buffer, defeating the larger-than-RAM design. It now streams
  close-delimited (no Content-Length) in bounded pages keyed by the
  query's total sort order, holding no engine lock across socket
  writes; equal-timestamp runs wider than a page grow the page until
  the cursor can cross them.

## [0.12.1] - 2026-07-23

### Fixed

- **Payload TTL deleted live data** (found in review, reproduced): the
  content-addressed store dedupes identical payloads to one file
  WITHOUT refreshing its mtime, while the TTL sweep deleted by mtime
  alone — a fresh span re-referencing old content kept its span but
  lost its payload. The sweep now protects every payload referenced by
  a live span (buffer + all segments, collected via the cached
  rollups) and deletes only unreferenced-and-old files.
- **LLM/session rollups double-counted replaced spans** (found in
  review, reproduced): cached per-segment rollups summed every
  physical copy of a re-ingested (trace_id, span_id), contradicting
  the primary key's last-write-wins semantics — the aggregate said
  2 calls / 30 tokens / $0.30 where the visible truth was
  1 call / 20 tokens / $0.20. Rollups now walk segments newest-first
  carrying the seen-key set (FNV-1a prefilter; buffer always wins);
  a segment containing any possibly-superseded key is re-scanned
  exactly, dropping stale versions. Collisions can only cost an
  unnecessary re-scan, never a wrong count.

## [0.12.0] - 2026-07-23

### Added

- **Payload offloading**: string attribute values above a threshold
  (server default 256 KiB, `--payload-threshold-bytes`, `0` disables)
  are extracted at ingest to a content-addressed store
  (`payloads/<aa>/<sha256>.bin`, temp+rename writes) and replaced by
  `{"$payload": "sha256/…", "bytes": N, "preview": "…"}`. Identical
  payloads are stored once; `GET /v1/payloads/{ref}` serves the bytes
  (hex-validated — traversal-shaped refs are 404). SHA-256 is
  implemented in-crate (FIPS 180-4) and verified against the NIST
  vectors.
- **Annotations**: post-hoc scores/feedback/eval verdicts attach to
  spans (or whole traces) without mutating them — an append-only,
  fsync'd `annotations.jsonl` with an in-memory index, tolerant of a
  torn tail. `POST /v1/annotations`, `GET /v1/annotations`, and the
  trace view carries a trace's annotations alongside its spans.
- **Dataset export**: `GET /v1/export` streams any span filter as
  NDJSON (unbounded by default, unlike interactive search) — the
  traces-to-eval-dataset path.
- TTL compaction now also drops annotations older than the window and
  sweeps payload files by mtime (an orphan payload outlives its span
  by at most one TTL).

## [0.11.0] - 2026-07-23

### Added

- **OTLP/HTTP binary protobuf**: `POST /v1/traces` now accepts
  `Content-Type: application/x-protobuf` — the encoding OTel SDKs use
  with `OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf` — via a
  dependency-free, bounds-checked wire decoder that lowers protobuf
  into the OTLP/JSON shape, so both encodings share one mapping.
  Protobuf clients receive a protobuf-typed empty
  ExportTraceServiceResponse. Malformed payloads (truncated varints,
  lying lengths, 32-deep hostile nesting) are 400s, never panics;
  unknown fields skip per the protobuf contract. gRPC is not served.
- **Span links**: spans carry a first-class `links` array
  (`trace_id`, `span_id`, `attributes`) on the native JSON surface and
  the OTLP mapping (previously dropped) — the non-tree structure of
  agentic traces: fan-out/fan-in, retries, cross-agent causality.
- Conformance suite with an independent test-side protobuf encoder
  (`tests/otlp_protobuf.rs`).

## [0.10.0] - 2026-07-23

### Added

- **Sessions**: any span carrying a `session.id` attribute joins a
  session — the unit of agentic/LLM work spanning many traces.
  `GET /v1/sessions` lists sessions (span/trace counts, token sums,
  cost, errors, activity window), `GET /v1/sessions/{id}` adds the
  per-trace breakdown. The dashboard grew a Sessions panel; clicking a
  session filters the span table to it.
- **LLM aggregation**: `GET /v1/stats/llm?group_by=model|service|session|day`
  returns exact token/cost/error/latency rollups over an optional
  `since`/`until` window. Numeric attributes are accepted as numbers or
  numeric strings; explicit `llm.total_tokens` wins over the
  prompt+completion sum.
- Aggregation cost model: sealed segments are immutable, so per-segment
  rollups are computed once and cached; query windows that split a
  segment decode just that segment for exact edge membership. Rollups
  for superseded (compacted) segments drop out automatically.
- `session.id` added to the LLM semantic conventions
  (docs/llm-semantics.md) with a sessions/aggregation section.

## [0.9.2] - 2026-07-23

### Security / Hardening

- The engine itself now rejects spans with an empty `trace_id` or
  `span_id` (`Error::InvalidSpan`) — the primary-key invariant no
  longer depends on each HTTP surface validating correctly, and a
  batch with any invalid span stores nothing.
- Socket read/write deadline (30s, `TRAZA_SOCKET_TIMEOUT_MS` to
  override): a peer that connects and goes silent — or declares a
  body it never sends — is released instead of parking a worker
  thread forever.
- Request headers are capped at 64 KiB (bodies keep the 64 MiB cap).
  Previously a request could spend the full body budget on headers,
  doubling the per-request memory ceiling.
- With auth enabled, requests are refused from the head alone:
  an unauthenticated client can no longer make the server buffer a
  declared 64 MiB body before hearing 401.

### Added

- `tests/ingest_hardening.rs`: adversarial wire probes — lying
  content-length, oversized headers, silent peers, pre-auth body
  refusal, malformed OTLP shapes, query-parameter extremes, inverted
  timestamps, hostile ids (NUL-prefixed / unicode) round-tripping
  through flush and reopen, and the documented last-write-wins
  semantics for duplicate keys within one batch.
- Test harnesses kill their spawned server on every exit path
  (`Drop`), so a failing test reports instead of hanging `cargo test`
  on the leaked child's pipes.

## [0.9.1] - 2026-07-23

### Fixed

- `POST /v1/spans` now rejects spans with an empty `span_id` (400,
  naming the span index), matching the existing empty-`trace_id`
  rejection. Both halves of the `(trace_id, span_id)` primary key must
  be non-empty: previously two distinct spans with empty `span_id`
  were both counted in `{"accepted": N}` while the upsert silently
  collapsed them into one stored span. The OTLP endpoint already
  rejected empty ids; the native endpoint now agrees. Rejection is
  atomic — nothing from a rejected batch is stored.

### Added

- `docs/ha-design.md`: the high-availability design document — four
  compared architectures with a quorum-replicated logical-log
  recommendation (segment shipping retained for catch-up), grounded in
  the real engine mechanisms (`WriteBuffer` acknowledgment boundary,
  `segment_v2` snapshot transfer, replicated supersede-journal
  transitions, `(trace_id, span_id)` idempotency, `DirectoryLock`
  scope). Design only; no HA behavior is implemented.

## [0.9.0] - 2026-07-23

### Added

- Bundled dashboard: a dependency-free trace browser embedded in the
  server binary (`src/dashboard.html` via `include_str!`), served at
  `GET /` and `GET /dashboard`. Recent-spans view with a filter bar
  mapped 1:1 onto the `/v1/spans` query params, a trace waterfall backed
  by `/v1/traces/{id}` with error spans highlighted, and a span detail
  pane (attributes, events, parent, extra fields). Light and dark color
  schemes follow the browser preference.
- The dashboard consumes only the existing JSON API — no new endpoints.
  With `TRAZA_TOKENS` set the shell stays open (it carries no data)
  while every API call remains gated; the page prompts for a bearer
  token on the first `401` and stores it in `sessionStorage` only.
- `traza::dashboard`: the embedded asset and route helper
  (`route(path) -> Option<DashboardResponse>`), consulted by the server
  before the auth gate for `GET` requests.
- `tests/dashboard.rs`: process-level acceptance — the real server
  serves the embedded page at `/`, `/dashboard`, and `/dashboard/`
  (unknown deeper assets 404); a grep oracle proves the page references
  no external URLs (self-contained, no supply-chain surface); with auth
  enabled the shell loads open while `/v1/*` still returns 401/403/200
  by scope.

### Changed

- README: Features and Roadmap now reflect shipped OTLP ingest, auth,
  LLM-observability semantics, and the dashboard; remaining roadmap is
  streaming results, filter throughput at scale, and high availability.

## [0.8.0] - 2026-07-23

### Added
- **Roadmap leg 4 — bearer-token auth.** `TRAZA_TOKENS` (comma-separated
  `scope:token`, scopes `ro`|`rw`) requires `Authorization: Bearer` on
  every request: unknown tokens 401 with a `WWW-Authenticate: Bearer`
  challenge, insufficient scope 403, token comparison constant-time (all
  credentials checked even after a match). Unset means open — the
  development default that keeps every existing test unchanged — while a
  set-but-invalid value refuses startup rather than silently running
  open. Zero new dependencies; process-level matrix tests cover open
  mode, 401/403/200 across ingest, OTLP, and flush endpoints, and the
  startup refusal.

## [0.7.0] - 2026-07-23

### Added
- **Roadmap leg 3 — LLM-observability semantics.** Documented gen-AI span
  conventions ([docs/llm-semantics.md](docs/llm-semantics.md)): `llm.*`
  span names, model/token/temperature/stop-reason/tool/cost attributes
  (index-served like any attribute), prompt and completion payloads as
  span events so large text never enters the filter index, and four
  concrete query recipes over the existing API. Process-level tests prove
  every recipe through both `/v1/spans` and OTLP ingest. Purely additive:
  one doc, one test target, one README section.

## [0.6.0] - 2026-07-23

### Added
- **Roadmap leg 2 — OTLP/HTTP JSON ingest.** `POST /v1/traces` accepts an
  OpenTelemetry ExportTraceServiceRequest in OTLP/HTTP JSON and maps it
  onto the span model: hex ids lowercased, `*TimeUnixNano` accepted as
  string or number, typed `AnyValue` attributes (string/int/double/bool/
  array/kvlist) flattened to plain JSON, resource `service.name` becoming
  the span's service (`unknown_service` fallback), scope attributes
  merging beneath span attributes, events mapped, and OTLP status codes
  becoming `ok`/`error`/empty. Structurally invalid requests 400 with a
  diagnostic; the existing `/v1/spans` contract is untouched. No new
  dependencies. Conformance-tested end to end against the real binary,
  including index-served queries over OTLP-ingested spans.

## [0.5.0] - 2026-07-23

### Changed
- **Roadmap leg 1 — larger-than-RAM reads.** Segments are file-backed:
  `Segment::open` reads only the header and index sections into memory and
  serves every record access by reading exactly the needed byte range from
  the file (std `Seek`+`Read`; no mmap, no new dependencies). Flushing
  reopens the new segment file-backed, so no resident payload copy survives
  the write either. `Store::resident_payload_bytes()` exposes the invariant
  (zero after open and after flush). Measured at a 10M-span corpus
  (2.4 GB on disk): **0.71 GB peak server RSS** (was ~2.4 GB
  bytes-resident, ~5 GB in the pre-v2 engine); trace lookup p50 0.8 ms,
  attribute filter p50 8.7 ms — RAM is O(indexes) and stores larger than
  memory serve correctly.
- The lazy limited-query merge caches per-source head timestamps: with
  file-backed segments every peek is a disk read, and re-peeking all
  sources per pop had regressed the 10M filter to 125 ms; cached heads
  restore 9.6 ms.

### Known deviation
- The leg's relative bound ("1M latencies within 2x of 0.4.0") is missed
  on filter p95: 3.34 ms vs 1.27 ms (2.6x) — the cost of on-demand file
  reads at sub-5 ms magnitudes. The absolute gate (< 300 ms) passes with
  ~90x headroom; recorded here rather than tuned away.

## [0.4.0] - 2026-07-23

### Changed (breaking)
- **Span identity is a primary key.** (trace_id, span_id) is enforced
  unique: re-ingesting an existing pair replaces the stored span — in the
  write buffer, across flushes, and across restart. Last write wins on
  every read path (trace, filtered, and limited lazy queries), so client
  retries are idempotent and never produce duplicate copies. This reverses
  0.3.1's at-least-once visible-duplicate semantics.
- **v1 JSONL segments are no longer read.** The engine is v2-only; opening
  a directory containing a legacy `.jsonl` segment fails loudly with a
  migration pointer (read with 0.3.x first). The dual-format code path is
  removed.

## [0.3.1] - 2026-07-23

### Fixed
- **Data loss across restart**: next-segment numbering only recognized
  `.jsonl` names, so a reopened v2-only store restarted at id zero and the
  next flush renamed over an existing segment, destroying persisted spans.
  Both suffixes count now, and `write_segment` refuses to replace an
  existing file outright.
- **Acknowledged duplicate cardinality survives restart**: content-based
  duplicate healing is gone. Compaction rewrites are journaled with a
  supersede marker written before the rewrite begins; recovery finishes an
  interrupted rewrite from the journal in either direction and never
  deduplicates by content, so legitimately re-ingested identical spans keep
  both acknowledged copies.
- A corrupt v2 header with an out-of-range attribute-index offset returns
  `Error::Corrupt` instead of panicking through unsigned subtraction.
- User attributes named with a NUL prefix (for example `"\u{0}service"`)
  can no longer overwrite the reserved service/name index keys and poison
  those queries; such attributes are stored verbatim but excluded from the
  index, and filters on them decline index use symmetrically.

### Changed
- **Limited queries are lazy end to end**: per-segment index postings stay
  undecoded and a k-way merge pops candidates in start-time order, decoding
  and re-verifying only what the limit returns. Measured: attribute filter
  p50 18 ms -> 0.53 ms at 1M spans and 209 ms -> 2.9 ms at 10M — the 10M
  advisory target (<100 ms) is closed with 35x headroom.
- README limitations and roadmap reflect the v2 engine (byte residency,
  journaled compaction, remaining mmap/streaming work).

## [0.3.0] - 2026-07-23

### Changed
- **Segment format v2**: new segments are indexed binary files (`.seg`) —
  JSON span payloads with an embedded record-offset index, trace index, and
  attribute index, written with the same temp + fsync + atomic-rename
  discipline. v1 JSONL segments remain fully readable beside v2 and heal
  through the same duplicate-recovery path; TTL rewrites produce v2.
- **Byte-resident reads**: opening a store no longer materializes spans.
  v2 segments hold raw bytes plus their indexes; spans parse on demand,
  only for records a query returns. `Store::resident_persisted_span_structs`
  exposes the invariant (zero after a v2-only open).
- **Index-served queries**: `get_trace` binary-searches the trace index;
  filters narrow through service/name/attribute indexes or time range and
  re-verify every predicate on the parsed span. Measured on the bundled
  benchmark: trace lookup p50 0.185 ms at 1M spans (was 14.2 ms) and
  0.536 ms at 10M (was 145.6 ms); attribute filter p50 18 ms at 1M
  (was 66.5 ms) and 209 ms at 10M (was 4,395 ms).

### Known limits
- The 10M advisory filter target (<100 ms) is not yet met: candidate
  payload parsing dominates large result groups. Posting-list
  intersection and parse-avoidance are the next optimization.
- Segment bytes are read into memory at open (no mmap yet); resident cost
  is file bytes + indexes rather than parsed structs.

## [0.2.3] - 2026-07-22

### Fixed
- Segment writes are buffered. `serde_json::to_writer` against a raw `File`
  issued one write() syscall per JSON token, making flush cost ~140 us per
  span and capping measured end-to-end ingest at 5,450 spans/s. A 256 KiB
  `BufWriter` restores flush to ~1.5 us per span; the regenerated benchmark
  measures 138,180 spans/s and the ingest gate passes again.

## [0.2.2] - 2026-07-22

### Fixed
- `ttl_seconds: Some(0)` disables expiration as documented instead of
  expiring every existing span; the library `Config` TTL default is `None`
  as documented, no longer a silent seven days.
- Recovery heals crash-duplicated segments: exact-duplicate spans are
  dropped at open and fully-duplicate segment files deleted, closing the
  window where a crash mid-compaction returned two copies of surviving
  spans on reopen.
- An empty reclamation sentinel left by a reclaimer that died before
  recording its PID no longer wedges lock recovery: unreadable sentinels
  older than ten seconds are treated as corpses.
- The README introduction and features no longer claim a manifest,
  per-segment indexes, or log replay; the configuration tables match the
  code (server `--ttl-seconds` drives engine compaction; `--host` and
  `--flush-spans` documented).

### Changed
- BENCHMARKS.md regenerated against the engine-backed server. The ingest
  gate is honestly reported as MISSED (5,450 spans/s against the 50,000
  target); read gates pass. Closing the write-path gap is the top of the
  roadmap.

## [0.2.1] - 2026-07-22

### Fixed
- The documented ingest contract works again: timestamp aliases
  (`start_time_unix_nano`, `start_timestamp_ns`, `start_ns`, `start_time`
  and the matching `end_*` keys) are accepted, `parent_span_id`, `status`,
  `attributes`, and `events` are optional, and unknown span fields are
  stored and returned verbatim instead of silently discarded. This also
  un-breaks the bundled benchmark, which emits `start_ns`/`end_ns`.
- The documented search filters work again: `attr.KEY`, `min_duration_ms`,
  `since`/`until`, and the default `limit` of 100.
- `/v1/stats` exposes the documented `span_count`, `segment_count`, and
  `bytes_on_disk` keys alongside the engine's finer-grained fields.
- The server binds `0.0.0.0` again by default; `--host` overrides.
- TTL expiration no longer empties the in-memory segment set when a file
  operation fails mid-compaction; the store keeps serving its previous view
  and surfaces the error.
- Stale-lock reclamation is single-winner: a reclamation sentinel closes the
  window in which a slow reclaimer could delete a fresh lock and defeat the
  single-writer guarantee.
- The README architecture section describes the engine as built (JSON-lines
  segments, memory-resident, linear scans, no manifest or per-segment index
  yet) instead of the roadmap design, and names the compaction
  crash-atomicity bound in Limitations.

## [0.2.0] - 2026-07-22

### Changed
- `traza-server` is engine-backed: the segment engine is the server's only
  datastore. The HTTP wire contract is unchanged; the server-side append-only
  log, in-memory indexes, and startup replay are removed, and restart
  durability and crash recovery are the engine's.

### Added
- `POST /v1/flush` forces buffered spans into a durable segment on demand.
- `--flush-spans` server flag to tune the engine's flush threshold; `--port 0`
  binds an ephemeral port announced on stderr.
- Five process-level `server_on_engine` integration tests that drive the real
  server binary end to end, including an engine-authority cross-check and a
  kill-and-restart persistence test.
- Stale-lock reclamation: `Store::open` reclaims a lock file whose recorded
  owner process is verifiably dead, so a crashed server cannot permanently
  wedge its data directory. A live owner still rejects the open.

### Fixed
- Deadlock-capable lock-order inversion between `flush()` and `stats()`; all operations now follow a documented writer-before-segments discipline.
- `query()` and `get_trace()` now take an atomic combined snapshot of buffered and persisted spans, so a concurrent flush can no longer hide committed spans.
- Crash-orphaned segment temp files no longer wedge subsequent flushes: temp names are unique per process, and `Store::open` removes orphans during recovery.
- Concurrent writers are rejected: `Store::open` holds a lock file for the store's lifetime and a second open fails with `Error::AlreadyOpen`.

### Added
- Concurrency and failure-injection tests: deadlock detection, read-during-flush consistency (exactly-once), stale-temp recovery, and second-open rejection. Direct per-segment ordering assertion in the flush test.

- Renamed the project and crate to Traza (`traza`).

## [0.1.0] - 2026-07-20

### Added

- The tracing storage engine exposed as a Rust library.
- The `traza-server` HTTP server for the documented ingestion and query endpoints.
- The `bench` executable for measuring the existing datastore workloads.
- Four behavioral integration tests: buffer-flush persistence, crash recovery via reopen, randomized filter equivalence against an independent naive reference, and TTL compaction. (Persisted batch *ordering* is asserted only indirectly; a direct segment-order assertion arrives with the storage-correctness work.)
- Crate documentation, dual MIT/Apache-2.0 licensing, and release automation.

### Known Limitations

- This is an initial 0.1 release; consult README.md for the currently documented operational constraints and unsupported use cases.

[Unreleased]: https://github.com/toshish/traza/compare/v0.22.2...HEAD
[0.22.2]: https://github.com/toshish/traza/compare/v0.22.1...v0.22.2
[0.22.1]: https://github.com/toshish/traza/compare/v0.22.0...v0.22.1
[0.22.0]: https://github.com/toshish/traza/compare/v0.21.0...v0.22.0
[0.21.0]: https://github.com/toshish/traza/compare/v0.20.0...v0.21.0
[0.20.0]: https://github.com/toshish/traza/compare/v0.19.0...v0.20.0
[0.19.0]: https://github.com/toshish/traza/compare/v0.18.0...v0.19.0
[0.18.0]: https://github.com/toshish/traza/compare/v0.17.0...v0.18.0
[0.17.0]: https://github.com/toshish/traza/compare/v0.16.0...v0.17.0
[0.16.0]: https://github.com/toshish/traza/compare/v0.15.0...v0.16.0
[0.15.0]: https://github.com/toshish/traza/compare/v0.14.0...v0.15.0
[0.14.0]: https://github.com/toshish/traza/compare/v0.13.0...v0.14.0
[0.13.0]: https://github.com/toshish/traza/compare/v0.12.2...v0.13.0
[0.12.2]: https://github.com/toshish/traza/compare/v0.12.1...v0.12.2
[0.12.1]: https://github.com/toshish/traza/compare/v0.12.0...v0.12.1
[0.12.0]: https://github.com/toshish/traza/compare/v0.11.0...v0.12.0
[0.11.0]: https://github.com/toshish/traza/compare/v0.10.0...v0.11.0
[0.10.0]: https://github.com/toshish/traza/compare/v0.9.2...v0.10.0
[0.9.2]: https://github.com/toshish/traza/compare/v0.9.1...v0.9.2
[0.9.1]: https://github.com/toshish/traza/compare/v0.9.0...v0.9.1
[0.9.0]: https://github.com/toshish/traza/compare/v0.8.0...v0.9.0
[0.8.0]: https://github.com/toshish/traza/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/toshish/traza/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/toshish/traza/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/toshish/traza/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/toshish/traza/compare/v0.3.1...v0.4.0
[0.3.1]: https://github.com/toshish/traza/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/toshish/traza/compare/v0.2.3...v0.3.0
[0.2.3]: https://github.com/toshish/traza/compare/v0.2.2...v0.2.3
[0.2.2]: https://github.com/toshish/traza/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/toshish/traza/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/toshish/traza/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/toshish/traza/releases/tag/v0.1.0
