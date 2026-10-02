# Praxis validation reuse

## Caller API

From a checkout containing the exact target commit, with `git`, Python 3 and `gh`
available:

```sh
python script/praxis-validation.py check \
  --repository OWNER/REPO --source-sha FULL_COMMIT_SHA --wait-seconds 300
```

Set `GH_TOKEN` to a token with `contents: read` and `actions: read`. The helper
appends exactly one `validated=true` or `validated=false` line to `GITHUB_OUTPUT`
when that environment variable is present, and prints the decision and original
run identity to stdout. A negative result, including an API/provenance error,
exits successfully: the caller **must run full quality unless the output is
exactly `true`**. A process-level failure must also fall back to full quality.
Argument-parser errors are process failures, not authorization to skip checks.

`--wait-seconds` defaults to zero. A verified bundle caller may specify up to
10800 seconds; other callers are limited to 600 (quality uses 300). This is a
total search/join deadline, not an extra wait after searching. With zero wait,
only history is searched, with a 60-second deadline. Each GitHub request has a
30-second timeout clipped to the remaining deadline.

The helper can join one matching PR attempt. An independent, verified bundle
caller can also join a direct push quality run with the same source tree and
workflow blob. Main quality never joins another push or a bundle, preventing
main/main and main/bundle wait cycles. Local/unknown callers cannot join pushes.
Polling performs one run-status request every 30 seconds, with the last delay
clipped to the deadline. It never dispatches, reruns, or cancels any run, and never
follows a superseding attempt. A successful final full receipt is mandatory.

The helper compares the local Git object's actual tree with GitHub's Git commit
API. It does not infer source identity from a branch name, a PR head SHA, or a
workflow run's successful conclusion alone. A complete checkout is not required,
but the target commit/tree must exist locally. No ZIP paths are extracted.

### Bundle integration

Merging a PR into `main` now starts the full bundle automatically after the
post-merge `Architect quality` run succeeds. `bundle_after_merge.yml` dispatches
`bundle_fork.yml` with the exact validated merge SHA and every platform enabled.
The bundle publishes the Praxis Dev self-update release only after all desktop
and remote-server builds succeed. Manual dispatch remains available.

The dispatcher does not check out source code. It accepts only successful quality
runs from a push to this repository's `main`, confirms the source is still the
current main tip and belongs to a merged main PR, and skips an already queued,
running, or successful full bundle with the same immutable source and build
configuration. Failed bundles can be dispatched again. Upstream sync is not a PR
merge and retains its own dispatch, including its `rebuild` option.

Bundle prepare invokes `check` with its pinned checkout SHA and `GH_TOKEN`; both
prepare and the reusable quality call need `actions: read` and `contents: read`.
The bundle wait budget is `--wait-seconds 10800`, inside a 190-minute prepare job
budget. Quality's own prepare remains `--quality-run --wait-seconds 300`. Sync
already grants sufficient permissions. Caller configuration is maintained in the
bundle workflow; the helper does not dispatch or modify either caller.

In Actions, the external helper recognizes only the direct `bundle_fork.yml`
dispatch caller. It checks the quality definition at the bundle's actual
`GITHUB_WORKFLOW_SHA`, corroborated against its run, because a relative reusable
call executes from that commit, not necessarily the selected source or today's
default branch. It additionally requires the default-branch quality definition to
match. Preserve the standard `GITHUB_*` execution environment; do not replace its
SHAs with `source_ref`. Unknown callers fail closed. Outside Actions, `check`
uses the default-branch definition as the expected execution definition.

Bundle's downstream validation gate accepts skipped quality **only when prepare's
`validated` output is exactly `true`**, not any skipped conclusion. If a joined
main run succeeded by reusing another validation, it has no full receipt. The
helper searches original receipts once more within the remaining search budget;
it never accepts that main run's skipped jobs as evidence.

## Quality workflow behavior

- PRs always execute all six existing, named quality jobs, even on identical
  source. Their required checks are never replaced by reused/skipped checks.
- Prepare resolves a moving source ref once; all six jobs check out that SHA.
- Direct non-PR quality uses `check --quality-run`, which verifies the actual
  executing quality definition before allowing reuse. It waits up to 300 seconds
  for a matching original PR validation when possible.
- `Quality source validated` runs even after failures/skips. It requires either
  verified reuse with all full jobs skipped, or success from every full job and
  the receipt job. Consider requiring this aggregate for non-PR protection.
- `record` captures PR number, base SHA, and head SHA from `GITHUB_EVENT_PATH`,
  validating event repository ownership and the immutable run head. API
  `run.pull_requests` associations are used only for PR number and ownership:
  their head/base SHAs can advance after the run, just like the live PR.
- `record` is internal to the receipt job. Only after all six jobs succeed can it
  write `receipt.json`, uploaded as
  `praxis-quality-full-RUN_ID-RUN_ATTEMPT` with 30-day retention. It independently
  checks the attempt's jobs through the API, including successful executed steps.
  Missing provenance can prevent receipt issuance without invalidating a full
  suite that actually passed. Upload failure still fails the aggregate.
- A reused run uploads no receipt. A run with skipped tests is never evidence for
  another reuse, even if an artifact is present. Successful originals, not
  recursive skip chains, are the only authorization.
- Only PR-number concurrency cancels superseded runs. Non-PR groups include run ID
  and attempt, avoiding cancellation between main, dispatch, and reusable runs.
- The Python unit tests run in Formatting. Older selected source without these
  scripts still runs the existing full suite but cannot issue a reusable receipt.

## Receipt verification and deliberate fallbacks

A schema-2 receipt records repository name/ID, source commit/tree, run
ID/attempt/event/head, workflow ID/path/execution commit/blob, and PR number plus
event base/head SHAs where applicable. Schema-1 receipts are not accepted because
they lack the captured event provenance. Acceptance requires all of the following:

- The run belongs to this repository and this workflow, completed successfully,
  and is still the same successful attempt after verification.
- The artifact belongs to that run/repository/head, is unexpired and bounded in
  size, and contains exactly one small regular `receipt.json` member. No paths,
  symlinks, duplicate members, or extra files are accepted.
- The six expected named jobs each completed successfully in that exact attempt,
  with successful steps and a known test step. Failed, cancelled, absent,
  ambiguous, and skipped jobs are rejected.
- GitHub's actual tested commit tree equals the local target tree, including
  workflow files, scripts, toolchain, and all other tracked content.
- The recorded executing workflow blob equals both the source definition and
  the consumer's expected executing definition. Comparing only the source tree
  is insufficient for a workflow dispatched with a different `source_ref`.
- A PR's tested commit is the synthetic merge: its two parents must equal the
  receipt's event base and the immutable run head SHA. The recorded event head
  must equal that same run head. The workflow execution SHA must be this tested
  merge SHA. The run's association supplies only PR number and repository
  ownership, corroborated against the live PR. GitHub can clear that association
  after merge; in that case the immutable run-head commit's associated PRs must
  contain the event/receipt's PR number with matching repository ownership.
  Missing, duplicate, conflicting, or inaccessible associations still fail closed.
  Neither API object's mutable base/head SHAs are provenance. Fork PRs are
  excluded; missing event fields, mismatched event heads, and invalid merge
  parents fail closed.
- Push/dispatch originals require selected source, run head, and executing
  workflow commit to be identical. A dispatch with a different selected source
  still runs quality, but does not mint a receipt with unverifiable input history.

**Reusable quality calls deliberately run in full and do not issue receipts.**
Their run identity and `github.workflow_*` context identify the caller, not
necessarily the called definition; this implementation does not pretend they
prove which reusable definition executed. This conservative fallback also covers
cross-ref/default-branch differences. Bundles can still reuse a verified original
PR merge tree directly, and a main push can reuse that same tree even when its
commit SHA differs from the synthetic PR merge SHA. Bundles can also wait for an
original direct main push and reuse its final full receipt. Sync's reusable
validation still issues no receipt: a later main or bundle may consequently need
another full validation. This is an explicit safe fallback, not proof of reuse.

History searches share a 60-second budget and at most 300 candidate inspections,
inside the total deadline. Each pass lists at most the newest 300 quality runs;
there is at most one additional pass after a successful joined run lacks a valid
full receipt. Failed/cancelled conclusions are filtered from listing metadata
without fetching run details. Successful candidates without receipt artifacts do
not trigger commit, job, or archive reads; rejected completed attempts are not
rechecked during the second pass. Exhausted budgets fall back to full validation.

A running PR's merge ref is only a wait hint: it must have two parents including
the immutable run head, and its tree and workflow blob must match the target.
The eventual receipt, not that mutable ref or association, proves the event base.
For running pushes, the immutable run head is the source and executing workflow
commit of the known direct workflow. The final full receipt remains mandatory:
a matching in-flight push might ultimately skip quality through reuse.

Missing/expired artifacts, API errors, rate limits, inaccessible synthetic merge
commits, insufficient permissions, malformed data, and older receipt formats all
cause full validation. This proves source and workflow-definition equivalence,
not hermetic equivalence of external services, mutable action tags, package
registries, or runner images.

## CI tests

```sh
python script/test-praxis-validation.py
```

The standard-library unittest suite mocks GitHub, Git object lookup, and time. It
covers event-backed merge parents and the reported mutable association on run
36875953557 (reviewed SHA prefixes with synthetic suffixes), workflow-definition
changes including the bundle's executing commit, invalid/fork provenance,
failed/cancelled/skipped jobs, missing artifacts, attempt changes, safe ZIP
reading, output/error contracts, bundle-only push joins, original-receipt
rescanning, deadline-clipped requests, bounded history, newer revisions,
read-only API calls, and the workflow's aggregate and concurrency contracts. Run it in CI; local validation for this task is limited
to `git diff --check`.
