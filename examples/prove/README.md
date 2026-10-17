# Prove demo

What the store committed, and proof it was not quietly changed: the
[ledger](../../docs/guide/ledger.md), an anchor, an erasure ledgered by
digest, an evidence pack, and one flipped byte named by offline verification.

```sh
examples/prove/run.sh
```

It builds `traza-server` if it is missing (or runs the binary `TRAZA_BIN`
names), starts it on port 8131 with one `admin` token, and deletes the
throwaway store on exit. `python3` recomputes every head and digest with
`hashlib` from the formulas in the ledger guide — nothing from Traza is used to
check Traza — and `curl` carries the requests.

## The beats

| Beat | What to watch for |
|---|---|
| 1 · ingest and checkpoint | Twelve spans in three support sessions, one of which (`support-2206`) will be erased. `POST /v1/checkpoint` publishes a generation. |
| 2 · the chain | `GET /v1/ledger/head` and the chain itself: genesis, then a checkpoint entry per published generation. `GET /v1/ledger/verify` answers `intact: true`. Then `ledger.jsonl` is read straight off the disk and every head recomputed with the guide's formula; the last must equal the server's head. |
| 3 · an anchor | A stand-in notary — an HMAC-SHA256 key kept outside the data directory — signs the head, and `POST /v1/ledger/anchors` records it (`201`). The same anchor naming a head the chain never recorded is refused `409`. |
| 4 · an erasure | `POST /v1/erasures` for the session. The chain gains a checkpoint entry for the generation that published the erasure, then the erasure entry, printed verbatim. Asserted: its `subject_digest` is `sha256` of the subject's canonical JSON, recomputed here, and the session id occurs nowhere in `ledger.jsonl`, byte for byte. |
| 5 · an evidence pack | `GET /v1/evidence`, its digest recomputed with the guide's standard-library script — a match — and the same pack with one count changed recomputed as a control — a mismatch. The erasure is listed `erased`, `conclusive`, and the session id is not in the pack. |
| 6 · offline, then damage | The server is stopped. `traza-server verify --ledger` exits `0`. One bit in the middle of the largest sealed segment the live manifest names is flipped, and the same command exits `2` naming `segment-….seg: digest mismatch`. The chain itself still recomputes, and the anchored head still matches the notary's copy: the damage is in the data, and re-digesting the files is what finds it. |

## Knobs

| Variable | Default | Meaning |
|---|---|---|
| `TRAZA_DEMO_PORT` | `8131` | Server port |
| `TRAZA_BIN` | `./target/release/traza-server` | The server binary; built with cargo when the default is missing |

## Honest caveats

- **The notary is a stand-in.** An anchor is worth exactly as much as the
  separation between the store and the place its countersignature and the
  signed head are kept. Here both are in the same throwaway directory as the
  store, one level apart, to show the shape. Traza stores the signature and
  never checks it; the demo checks it with the key.
- **The demo does not show the chain resisting a rewrite, because it does
  not.** The chain has no secret: whoever can write `ledger.jsonl` can rewrite
  an entry, recompute every head after it, and verification will report the
  result intact. Only a head kept outside the store — the anchor — catches
  that. See [what it proves, and what it does not](../../docs/guide/ledger.md#what-it-proves-and-what-it-does-not).
- **A subject digest hides only an unguessable subject.** `support-2206` is
  a sequential id: anyone who sees the full entry can confirm it by guessing.
  The demo asserts the id is not written down, not that it cannot be guessed.
- **The flipped byte is found because it is in a manifested file.** The
  ledger makes committed history tamper-evident; it does not make the live
  directory tamper-proof. A file altered between two checkpoints is digested
  as found by the next one.
- The offline check runs on a stopped server because it must: it takes the
  directory lock and refuses a directory a live server owns.
  `GET /v1/ledger/verify` performs the same file check live.

## Runtime

About 2 s on an Apple M1 Max laptop. There is no scale knob: CI runs it as is.
