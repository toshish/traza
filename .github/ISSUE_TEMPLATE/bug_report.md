---
name: Bug report
about: Something behaves differently than documented
labels: bug
---

**Version and platform**

`traza-server --version`, OS/arch, and how you installed it (release
archive, container, cargo, source).

**What happened**

The command or request you ran, what came back, and what the documentation
says should have come back. Server stderr around the moment helps; startup
logs every path decision it makes.

**Reproduction**

Smallest sequence that shows it — ideally against a fresh `--data-dir`. If
it only reproduces against your existing store, say roughly what shape that
store is (span count, batch sizes, upserts or not).
