# Comment Triage

Unresolved: 1 | Resolved: 15 | Outdated: 6

## Unresolved

### Greptile — minor

- [ ] **T22** `crates/atc-cli/src/health.rs:220` @greptile-apps[bot]
  > <a href="#"><img alt="P2" src="https://greptile-static-assets.s3.amazonaws.com/badges/p2.svg?v=7" align="top"></a> **Records refreshed by 7B never trigger cost warnings in 7A**
  > Section 7B is intentionally not gated on `r.changed` — it picks up stale records that were never processed, even if their status didn't change in this health run. After 7B runs `run_post_completion`, it re-reads the freshened record from the registry (updating `entry.record` including `cost_usd`), then 7A checks:
  > if !r.changed {
  > continue;
  > }
  > Because only `entry.record` is updated (not `entry.changed`), any stale record processed by 7B where `r.changed == false` is silently skipped in 7A. These are precisely the records most likely to have unusual cost values — ones whose watcher died before post-completion ran. The cost warning for such a record will never fire: on subsequent health runs, `artifacts` will be set (7B won't run again) and `r.changed` will remain `false` (no new transition).
  > Consider tracking which records 7B refreshed and explicitly including them in the 7A warning pass:
  > // --- 7A: Cost threshold warnings ---
  > let cost_threshold = config.health.cost_warning_threshold;
  > let refreshed_id_set: std::collections::HashSet<&str> =
  > refreshed_ids.iter().map(|s| s.as_str()).collect();
  > for r in &results {
  > // Warn if the record just transitioned OR if 7B just ran post-completion for it
  > if !r.changed && !refreshed_id_set.contains(r.record.id.as_str()) {
  > continue;
  > }
  > if let Some(msg) = cost_warning(&r.record, cost_threshold) {
  > emit(json, &msg);
  > }
  > }
  - ID: 2972458522 | Thread: PRRT_kwDORljCNM52A8hQ
  - Reply: `gh api repos/harmony-labs/atc/pulls/14/comments/2972458522/replies -f body='...'`
  - Resolve: `gh api graphql -f query='mutation{resolveReviewThread(input:{threadId:"PRRT_kwDORljCNM52A8hQ"}){thread{isResolved}}}'`

## Review Summaries

**@coderabbitai[bot]** — COMMENTED:
> **Actionable comments posted: 1**

<details>
<summary>🤖 Prompt for all review comments with AI agents</summary>

```
Verify each finding against the current code and only fix it if needed.

Inline comments:
In `@tests/bats/helpers/common.bash`:
- Around line 171-178: The subshell that runs the test git setup does not check
the result of cd "$TEST_TMPDIR/worktree", so if cd fails the subsequent git
commands run in the wrong directory; update the subshell to guard the cd by
checking its exit (e...

## Resolved

<details><summary>15 resolved threads</summary>

- [x] **T2** `crates/atc-cli/src/health.rs:110` @greptile-apps[bot] — <a href="#"><img alt="P1" src="https://greptile-static-assets.s3.amazonaws.com/badges/p1.svg?v=7" align="top"></a> **Unbounded duplicate dispatches on every health run**
- [x] **T5** `crates/atc-cli/src/health.rs:262` @greptile-apps[bot] — <a href="#"><img alt="P2" src="https://greptile-static-assets.s3.amazonaws.com/badges/p2.svg?v=7" align="top"></a> **Destructive worktree cleanup is not gated by `auto_enabled`**
- [x] **T6** `crates/atc-cli/src/health.rs:262` @mateodelnorte — Fixed in 182e60d — worktree cleanup now skips records whose worktree directory no longer exists, avoiding unnecessary `gh pr view` API calls for already-cleaned entries.
- [x] **T7** `crates/atc-cli/src/health.rs:199` @greptile-apps[bot] — <a href="#"><img alt="P1" src="https://greptile-static-assets.s3.amazonaws.com/badges/p1.svg?v=7" align="top"></a> **Section 7B fires `run_post_completion` on NeedsReview→Done/Failed transitions, causing duplicate notifications**
- [x] **T8** `tests/bats/lifecycle.bats:382` @greptile-apps[bot] — <a href="#"><img alt="P1" src="https://greptile-static-assets.s3.amazonaws.com/badges/p1.svg?v=7" align="top"></a> **Auto-review trigger BATS tests will silently never fire in a no-repo environment**
- [x] **T9** `crates/atc-cli/src/health.rs:197` @coderabbitai[bot] — This fallback executes on every health run, but `crates/atc-core/src/post_completion.rs` (`run_post_completion`, lines ~39-184) unconditionally calls `cleanup_if_pr_done` once it has a PR URL. That means a plain `atc health` can still delete worktrees for stale records even when `--auto` is off. Split artifact extraction from PR cleanup, or plum...
- [x] **T10** `crates/atc-cli/src/health.rs:197` @coderabbitai[bot] — `crates/atc-core/src/post_completion.rs` (`run_post_completion`, lines ~39-184) persists recovered PR/cost data, but the rest of this function keeps using the pre-extraction `results` snapshot. So the same invocation can miss auto-cleanup, skip auto-dispatch, and emit stale `--json` output for a record whose PR URL was only recovered from the lo...
- [x] **T11** `crates/atc-cli/src/health.rs:262` @coderabbitai[bot] — This loop only inspects `results`, but later in the function you still need `registry.list(...)` to pull terminal rows for display. A dispatch that reached `Status::Done` before this invocation is therefore invisible here, so if its PR is merged/closed afterward, subsequent `atc health --auto` runs will never clean up the worktree. Load eligible...
- [x] **T13** `crates/atc-cli/src/health.rs:199` @greptile-apps[bot] — <a href="#"><img alt="P1" src="https://greptile-static-assets.s3.amazonaws.com/badges/p1.svg?v=7" align="top"></a> **Section 7B fires duplicate notifications when log has no cost data**
- [x] **T14** `tests/bats/helpers/common.bash:181` @greptile-apps[bot] — <a href="#"><img alt="P1" src="https://greptile-static-assets.s3.amazonaws.com/badges/p1.svg?v=7" align="top"></a> **Silent failure in CI when git user identity is not configured**
- [x] **T16** `crates/atc-cli/src/health.rs:198` @greptile-apps[bot] — <a href="#"><img alt="P2" src="https://greptile-static-assets.s3.amazonaws.com/badges/p2.svg?v=7" align="top"></a> **Silent skip when log file is missing in section 7B**
- [x] **T17** `crates/atc-cli/src/health.rs:109` @coderabbitai[bot] — `dispatch::dispatch()` treats `DispatchOpts.slug` as a git-kb task slug (`resolve_mode` and `cas_claim` both use it that way). When `task_slug` is missing, the current fallback turns every non-task candidate into a guaranteed failed auto-dispatch instead of skipping it cleanly.
- [x] **T19** `crates/atc-cli/src/health.rs:261` @greptile-apps[bot] — <a href="#"><img alt="P2" src="https://greptile-static-assets.s3.amazonaws.com/badges/p2.svg?v=7" align="top"></a> **7C makes O(N) sequential `gh pr view` calls on every `--auto` run**
- [x] **T20** `crates/atc-cli/src/health.rs:305` @greptile-apps[bot] — <a href="#"><img alt="P2" src="https://greptile-static-assets.s3.amazonaws.com/badges/p2.svg?v=7" align="top"></a> **Dispatch error message ignores the `json` flag**
- [x] **T21** `crates/atc-cli/src/health.rs:192` @greptile-apps[bot] — <a href="#"><img alt="P1" src="https://greptile-static-assets.s3.amazonaws.com/badges/p1.svg?v=7" align="top"></a> **Cost warnings fire on every `atc health` call, not just on first detection**

</details>

## Outdated

<details><summary>6 outdated threads</summary>

- [x] **T1** `crates/atc-cli/src/health.rs:174` @coderabbitai[bot] — The code comment says "For Running records where the agent exited" but the condition checks for `Done | Failed | NeedsReview` statuses. This is correct behavior since `HealthChecker::run()` transitions Running records to terminal states, and `r.changed` indicates the record was just transitioned. The post-completion extraction will correctly ski...
- [x] **T3** `crates/atc-cli/src/health.rs:150` @greptile-apps[bot] — <a href="#"><img alt="P2" src="https://greptile-static-assets.s3.amazonaws.com/badges/p2.svg?v=7" align="top"></a> **Misleading comment for stale-record detection**
- [x] **T4** `crates/atc-cli/src/health.rs:228` @greptile-apps[bot] — <a href="#"><img alt="P2" src="https://greptile-static-assets.s3.amazonaws.com/badges/p2.svg?v=7" align="top"></a> **Section labels are out of sequential order**
- [x] **T12** `tests/bats/helpers/common.bash:178` @coderabbitai[bot] — ShellCheck correctly flags that if `cd` fails, the subsequent `git` commands would execute in the wrong directory. While this is within a subshell (limiting damage), it's still best practice to guard against silent failures in test setup.
- [x] **T15** `crates/atc-cli/src/health.rs:145` @mateodelnorte — Fixed in e689fb3 — cost warnings (7A) and auto-review progress messages (7D) now route to stderr when `json` is true, keeping stdout parsable.
- [x] **T18** `crates/atc-cli/src/health.rs:149` @coderabbitai[bot] — `run_post_completion()` can be the first place `cost_usd` gets populated for watcher-missed records. Because the warning pass runs before `results` is refreshed, expensive dispatches that transition straight to `Done` or `Failed` here will never warn, and later `health` runs will not revisit them in `results`.

</details>
