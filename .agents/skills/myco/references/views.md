# Views — board / report / epic / milestone / index / org

Read-only lenses over the ledger. All accept `--json`; none mutate
(except `index` maintenance commands, which only touch the derived
cache — never the ledger).

## Single repo

```bash
myco status [--prompt]     # digest; --prompt is the token-sized agent form
myco board                 # kanban: Backlog | Open | Claimed |
                           #   In Progress | Blocked — fits the
                           #   terminal, top ~25 rows
myco board --all           # every card, no row cap
myco board --md            # same, as a markdown table (docs/READMEs)
myco board --epic v2 --milestone 0.2   # filtered lanes
myco report                # aging + velocity + blocked — the trend
```

`board` answers "what's in flight right now"; `report` answers "how
is the ledger trending" (stale items, throughput, stuck work) plus
"stale claims" — `in-progress` items idle >3d, interrupted in
practice. A `⚡` marks items whose latest handoff is an interrupt
(`myco interrupt`). Human boards shrink to the terminal width
(`$MYCO_WIDTH` overrides) and cap at ~25 rows — `--all` lifts the
cap; `--md`/`--json` always emit everything. List verbs take
`--limit N` for the same job. On a tty,
long output pages through `$MYCO_PAGER`/`$PAGER`/`less -FRSX`;
piped or agent output never pages.

## Groupings — `epic` / `milestone`

Items carry `--epic`/`--milestone` names (`[A-Za-z0-9_.-]+`, set at
create or via `item set`).

```bash
myco epic                  # all epics with counts
myco epic v2               # board filtered to that epic (≡ board --epic v2)
myco milestone             # all milestones with counts
myco milestone 0.2         # that milestone's board
```

Bare name lists groups; a name renders the group as a board. `--md`
emits the table form.

## Cross-repo — `index` + `org`

The index is a disposable derived cache (redb under
`~/.local/share/myco`, `$MYCO_HOME` overrides) — rebuild freely, the
markdown stays canonical.

```bash
myco index add <repo-path>     # register + snapshot (root or project/ path)
myco index list                # registered repos + snapshot freshness
myco index rebuild [<path>]    # re-scan one or all
myco index remove <path>       # unregister

myco org status                # per-repo counts + totals + by-owner
                               #   (idle repos hidden; --all shows them)
myco org board                 # org kanban, repo-prefixed cards
myco org board --repo thal     # one repo's cards only
```

`org` views auto-rebuild stale snapshots — register each repo once
with `index add`, then `org status` is the cheap org-wide answer to
"what am I working on everywhere".

## When which

- Session start → `status --prompt` (see continuity.md).
- "Show me the work" / standup → `board` or `report`.
- "What's left in this push" → `epic <name>` / `milestone <name>`.
- Multiple repos in play → `index add` once per repo, then `org`.
