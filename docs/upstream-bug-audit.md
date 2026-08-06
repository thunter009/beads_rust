# Upstream Bug Audit — fork-only fixes vs Dicklesworthstone/beads_rust

Audit performed 2026-04-28 against `origin/main` at `7865fae`. Snapshot of the
59 commits this fork holds ahead of upstream main, classified by whether each
fix corresponds to a bug that still exists upstream.

> **2026-08-05 — the fork's code is retired. This branch is now upstream plus
> this document, nothing else.**
>
> - **The open-child guard was superseded upstream.** `6753c34e`
>   ("fix(close): guard against silently orphaning dot-notation children on
>   parent close") landed in **v0.2.0**, 2026-04-22 — six days before this audit
>   was written, which is why the audit still lists `92831ed`/`995daee` as
>   fork-specific. Upstream's version is strictly broader: it partitions
>   requested vs unrequested children and previews the first five. Verified
>   against real data on v0.2.19 — closing a parent with open children is
>   refused (`epic has 12/54 open children (use --force to close anyway)`) and
>   the parent stays open. Both fork commits were dropped in the rebase that
>   produced this revision.
> - **Do not run v0.2.20.** It makes a workspace unusable after the first
>   command: `br init` succeeds, then every later command fails with
>   `Database error: database is busy` — on a freshly-created v17 DB, single
>   process, no JSONL. Reproduced from a source build, so it is not a bad
>   release artifact. Cause is the `inspect_pending_sync_merge_under_authority`
>   gate from `251b501b`, which `git tag --contains` places in **v0.2.20 only**.
>   Filed upstream as **#412**.
> - The same gate masks `SchemaMismatch` as "busy" for pre-v17 databases, so
>   `main.rs`'s `reviewed_schema_migration_required` route is unreachable and
>   such a DB cannot be opened, migrated, or repaired (`br doctor --repair`
>   refuses through the same gate). Also in #412.
> - **Pinned to v0.2.19.** Its `CURRENT_SCHEMA_VERSION` is 16, which is what our
>   databases already are, so it opens them with no migration. Note upstream
>   #411: the v0.2.19 release checksum does not verify against the documented
>   Minisign key — build from the tag rather than downloading the artifact.
>
> The candidate tables below were written against `7865fae` and were **not**
> re-verified against current upstream. Treat every row as unconfirmed until
> re-checked — as the open-child entry shows, upstream may have fixed a bug in
> unrelated work since.

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
| 92831ed, 995daee | ~~open-child guard (PR #260 closed without merge)~~ — **misclassified; superseded upstream by `6753c34e` in v0.2.0. Both commits dropped 2026-08-05.** PR #260 was indeed closed unmerged, but upstream implemented an equivalent (broader) guard independently. |
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
