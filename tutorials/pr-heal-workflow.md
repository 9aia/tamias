# Tutorial: heal a failing PR with an agent

A guided walk from `tamias install` to a pull request that fixed
itself: red CI → agent session repairs the code → push → green check →
campaign merges. This is the `pr-heal` workflow bundle, verified
end-to-end — every command below ran for real.

You will need: `tamias`, `peregrin`, `sepia`, `lasius`, `bupha`, `gh`
(on `PATH`, authenticated), and a GitHub repo with a CI check.

## 1. Install the bundle

`pr-heal` ships in the 9aia workspace repo:

```sh
tamias install --allow-drivers ~/GitHub/9aia/workspace/workflows/pr-heal
```

`--allow-drivers` is required for `peregrin-bundle` content — the
bundle renders a systemd service through the peregrin provider.

## 2. Deploy it

```sh
tamias deploy pr-heal
```

This writes `~/.config/peregrin/serve/pr-heal.yaml` — a campaign
document that `include:`s the bundle's `main.yaml` and carries a
`config:` block. The loader deep-merges that block over the bundle
defaults, so **this file is where you configure the deployment**:

```yaml
config:
  name: pr-heal
  public_url: https://tamias.luisfuturist.com
  repo: "you/your-repo"                    # required
  webhook_secret_env: "BUPHA_WH_YOU_YOUR_REPO"  # required
  checkout: "$HOME/GitHub/you/your-repo"   # required — local clone
  agent: "devin"                           # sepia agent-config id
  agent_mode: "bypass"                     # see §6 — required
  max_agent_attempts: 3
  ci_check_name: "ci"
  default_branch: "main"
```

If the webhook secret env var is not already in
`~/.config/peregrin/env`, also declare its name under
`capabilities.secrets` in the same file.

> `tamias deploy` regenerates this file — keep a copy of your config
> block and re-apply it after re-deploys.

## 3. Register the webhook

Point a GitHub webhook at the shared campaign listener (the URL your
deployment's tunnel fronts — `https://tamias.luisfuturist.com/hook`
on this one):

- events: `push`, `pull_request`, `check_run`, `status`
- secret: the value behind `webhook_secret_env`, stored in
  `~/.config/peregrin/env` as `BUPHA_WH_<OWNER>_<REPO>`
  (uppercased, `-` → `_`)

```sh
gh api repos/you/your-repo/hooks -f name=web -f 'config[url]=https://tamias.luisfuturist.com/hook' \
  -f 'config[content_type]=json' -f 'config[secret]=…' \
  -f 'events[]=push' -f 'events[]=pull_request' -f 'events[]=check_run' -f 'events[]=status'
```

## 4. Restart and watch it come up

```sh
systemctl --user restart peregrin-campaign
journalctl --user -u peregrin-campaign -f | grep pr-heal
```

Every `peregrin-bundle` renders into the one shared
`peregrin-campaign.service` — ci-gate, docs-deploy and pr-heal
campaigns coexist on a single webhook listener that demuxes by repo.

## 5. Run the loop

Open a PR that breaks CI (the spike used a `SPIKE_BROKEN` marker that
`ci.sh` rejects):

```sh
gh pr create --title 'deliberate CI failure' --body 'pr-heal e2e'
```

Then watch the chain — no polling anywhere, every hop is a webhook or
entity event:

```
push/pull_request webhook → ci-job entity → bupha CI failure
  → status event → pr-heal.check.failed.<sha>
  → lasius worktree @ sha → sepia agent session fixes the tree
  → lasius commit + push HEAD:refs/heads/<pr-branch>
  → CI re-runs → green check → lasius forge pr merge --squash
```

Real observed output from the verified run (PR #3 of the spike repo):

```
{"merge":"9fcfce2c2de642b68fa55b8f013a3dba530ceb04",
 "mergedAt":"2026-11-11T22:48:17Z","state":"MERGED"}
```

Inspect what happened:

```sh
tamias logs pr-heal                          # campaign service logs
tail -f ~/.local/share/peregrin/serve-history/pr-heal.jsonl   # event log
ls ~/.local/share/pr-heal/worktrees           # heal worktrees (auto-removed on success)
cat ~/.local/share/pr-heal/worktrees/attempts/pr-3            # attempt ledger
```

## 6. The agent permission mode — read this

`agent_mode: "bypass"` is not optional decoration: unattended agent
turns cannot answer ACP `session/request_permission` prompts, and
devin's default `accept-edits` mode prompts on shell commands — the
turn deadlocks until timeout (observed live, twice). `bypass`
auto-approves everything, which is acceptable *because* the agent only
ever touches a disposable worktree. `smart` is the safer middle
ground. T-023 added a proper per-session `permissions` policy on the
sepia wire (`sessions.set_permission_policy`, rules over tool kinds) —
the campaign will migrate to it; until then `bypass` is the honest
setting for unattended heals.

## 7. Safety model, honestly stated

- **Verified-sha merge** — the campaign only merges when an open PR's
  *current* `head_sha` equals the sha that went green. A stale green
  finds no PR and skips.
- **Per-PR attempt ledger** — `<worktree_base>/attempts/pr-<n>` caps
  agent runs (default 3), durable across restarts. Over-cap reports
  `attempts.gaveup` and leaves the PR for a human.
- **Per-sha agent lock** — duplicate failure events can't start
  parallel agents on one worktree.
- **Merge conflicts are NOT auto-resolved** — `merge.failed` is
  terminal; conflict → agent-rework is tracked follow-up work.
- **No-diff fixes fail honestly** — the push node commits without
  `--allow-empty`, so an agent that changed nothing errors instead of
  looping CI forever.

## What just happened, architecturally

| hop | owner |
| --- | --- |
| webhook verify, parse, repo demux | peregrin (`github-webhook` driver, `push_refs`/`*_actions` filters) |
| CI job queue → worktree → status | bupha (`ci-job` entity plane) |
| agent session, prompt, permission mode | sepia (`/api/sessions/run` → `prompt_wait`) |
| worktree, commit, push, PR merge | lasius (`vcs`/`forge` verbs — `gh` is the impl) |
| install, deploy, service lifecycle | tamias (this bundle) |
