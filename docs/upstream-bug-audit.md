# Upstream Bug Audit — fork-only fixes vs Dicklesworthstone/beads_rust

Audit performed 2026-04-28 against `origin/main` at `7865fae`. Snapshot of the
59 commits this fork holds ahead of upstream main, classified by whether each
fix corresponds to a bug that still exists upstream.

## Status at audit time

- Upstream issues filed from this audit:
  - **#267** — `br sync --rebuild --rename-prefix wipes the DB after successful import`
  - **#268** — `br sync tombstone preservation: drops labels/deps/comments; cleanup deletes preserved tombstones`
- Upstream PR #260 (open-child guard) was closed without merging — guard
  logic is fork-only, so the self-reference fix on
  `fix/close-self-reference` (commit `5c95dd3`) does not have an upstream
  counterpart.

## Categories

- **A** — references an already-handled upstream issue (skip).
- **B** — fork-specific code path; bug only exists because the fix targets
  fork-only logic upstream doesn't have (skip).
- **C** — not a bug (refactor, style, test, doc, dependency bump, release chore).
- **D** — real bug, upstream-applicable, no existing upstream issue (candidate).
- **E** — real bug, upstream-applicable, already-filed-and-OPEN issue (skip with comment).
- **F** — real bug, upstream-applicable, prior issue closed without fix (file fresh).

## Filed candidates

| sha | category | upstream issue |
|---|---|---|
| ff1d5e7 | D | **#267** filed 2026-04-28 |
| ee5bc69, 1bedd4ff, a0d95ee, 68e2bf65, 31f9b728, 97a085c | D (umbrella) | **#268** filed 2026-04-28 |

## Candidates not yet filed

If maintainer engages with #267/#268, consider filing some of these as
follow-ups. Each row has the commit body's symptom + fix shape captured;
re-verify against current upstream source before filing — the audit
checked symptom presence, not whether upstream silently fixed the bug
in unrelated work.

### Independent standalones

| sha | subject | severity | notes |
|---|---|---|---|
| 38694a2 | `br sync --flush-only` skips `.write.lock`, can deadlock | concurrency hazard | adjacent to closed #243 |
| 0b79255 | doctor reports "JSONL not found" for parse-error JSONL | diagnostic clarity | low priority |
| 0abd680 | duplicate issue IDs in JSONL silently collapse last-write-wins | silent data loss | 4 ingest paths affected |
| 98152e9 | `-vv`/`-vvv` flood with fsqlite per-row chatter | usability | medium priority |
| f937693 | error panel renders without error border | cosmetic | low priority |
| 420e950 | `br reopen` leaves `close_reason` + `closed_by_session` populated | export/changelog correctness | medium priority |
| 27da9a3 | `br changelog --since-tag` prints timestamp instead of tag; empty-validation panic | usability + crash | medium priority |
| 54e3ff8 | bottleneck list names blocked issue as bottleneck; misses 3 dep types | analytics correctness | high — wrong-direction reports |
| c9fd02a | id-resolver substring match crosses prefix namespaces | correctness | **fork-only — `src/id_resolver.rs` doesn't exist upstream**; SKIP |

### Conflict-marker cluster

Three commits guarding sync paths against unresolved-merge JSONL.

| sha | subject |
|---|---|
| 986bfb89 | `br sync --merge` gives cryptic "Invalid JSON" on conflicted base snapshot |
| 4dd4790 | post-command auto-flush silently overwrites JSONL with conflict markers |
| 79e3a4f | `br sync --flush-only` main path silently rewrites conflicted JSONL |

Could file as one umbrella (`merge-conflict markers cause silent overwrite or
cryptic errors across multiple sync paths`).

### Deferred-recovery / rename-prefix cohort

Nine commits covering missing-DB + `--rename-prefix` + mode-flag
interactions. Several are catastrophic data-loss combinations.

| sha | subject |
|---|---|
| cb7c78e | `--rename-prefix` on missing DB rebuilds without applying rename |
| 0821b00 | `--rebuild` auto-recovery shortcut silently drops `--rename-prefix` |
| 2e8a3ce | failed deferred-recovery import leaves empty DB; pre-recovery backup orphaned |
| 56e965e | `--merge` / `--flush-only` failure after deferred-recovery leaves empty DB |
| d345fae | post-restore `OpenStorageResult` holds in-memory handle, not restored file |
| 90105c8 | bad `--jsonl` path moves user's DB into `recovery_dir/` before validation rejects it |
| 047fd70 | `--rebuild` redundantly rebuilds after open-time auto-recovery already rebuilt |
| f97e8ea | invalid sync mode-flag combinations rebuild DB before validation rejects them |
| ff1d5e7 | filed as #267 |

## Fork-specific (Category B) — never to file

These fixes target fork-only logic upstream doesn't carry. Listed here so a
future audit doesn't reclassify them.

| sha | reason fork-specific |
|---|---|
| 92831ed, 995daee | open-child guard (PR #260 closed without merge) |
| 5c95dd3 | self-reference fix on the open-child guard |
| 33a06bab, dcf3756, 3546f15, f9660535, 760d393, 4035f9b8 | fsqlite-specific workarounds; canonical fix lives in frankensqlite |
| c9fd02a | `src/id_resolver.rs` doesn't exist in upstream |

## Already-handled (Category A) — referenced and CLOSED upstream

| sha | upstream issue |
|---|---|
| bcc7195 | #256 |
| 87e0b4b | #256 |
| 3b124d04 | #245, #248 |
| b7eba76 | #254, #255 |
| 21f4bcfa | #244 (upstream landed equivalent fixes via `112cf2d` + `f334e62`) |

## Method

```
# Commits ahead of upstream
cd /Users/thom/20-29_tools/beads_rust
git log fork/main ^origin/main --pretty=format:"%H %s"

# Compare individual code paths
git show origin/main:<path>   # vs fork's HEAD on the same path
```

Source-level verification is required before filing — the audit's table
identifies symptom candidates, not confirmed upstream presence. The two
already-filed issues (#267, #268) verified upstream code by reading
`origin/main`'s `src/cli/commands/sync.rs` to confirm the buggy shape was
unchanged.
