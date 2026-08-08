## What changed, and why

<!-- The narrative belongs here: what was wrong or missing, what this does
     about it, and what a reviewer should look hardest at. -->

## Verification

- [ ] `./ci.sh` is green locally
- [ ] Every surface this touches is documented in this same PR (docs ship with the change, not after it)
- [ ] Performance claims come with a benchmark run attached — or the PR makes none
- [ ] If this touches durability, the WAL, the segment format, or wire responses: the invariant concerned ([docs/internals/invariants.md](../docs/internals/invariants.md)) is named above, and a test exists that fails when it breaks
- [ ] If this adds a dependency: the written reason is in the PR (see [CONTRIBUTING.md](../CONTRIBUTING.md))
