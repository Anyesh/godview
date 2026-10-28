# godview

A [herdr](https://herdr.dev) plugin pane that shows every agent in your herdr session as one live tree, with each Claude Code agent's subagents and workflows nested under it.

```
 godview   18 agents · 5 working · 2 subagents running                               m motion · w working only · a all
┌ agents ─────────────────────────────────────────────────────────────────────────┐┌ worker-3 · beta · incr ──────────┐
│▶ ▾ incr                                                                         ││Integrator handoff                │
│    ◓ worker-3     beta      44%  ▁▂▅▇▅▃▂▃▅▇  Integrator handoff                 ││context 44% (438k) · cache 99.7%  │
│                                          ··• Agent  Review T10 scan-two di… 20s ││                                  │
│      ┣ ● general-purpose Apply CHANGELOG audit list               Edit       4s ││recent tools                      │
│      ┃ └ ● Explore         Find stale CHANGELOG refs              Grep       1s ││  20s  Agent  Review T10 scan-two │
│      └ ● code-reviewer   Review T10 scan-two diff                 Read      20s ││   4s  Read   crates/sched/lib.rs │
│    ◑ worker-5     gamma     27%  ▁▁▂▂▃▂▁▂▃▂  886d3 op22 crosstwo both-panic     ││                                  │
│                                              Read  src/sched/cross.rs        3s ││last text                         │
│    ◐ worker-9     beta      35%  ▅▃▁▁▁▁▁▁▁▁  Incr T8 shared-hang worker-9       ││ Audit subagent is still running. │
│                                              Bash  cargo nextest run         4m ││                                  │
└─────────────────────────────────────────────────────────────────────────────────┘└──────────────────────────────────┘
┌ live ───────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
│   0s  worker-5                    Read   src/sched/cross.rs                                                         │
│   1s  worker-3 › Explore          Grep   stale CHANGELOG refs                                                       │
│   4s  worker-3 › general-purpose  Edit   CHANGELOG.md                                                               │
│   9s  harness › wf review:app     Read   src/app.rs                                                                 │
└─────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
 j/k move · enter go to pane · p peek · space fold · m motion · q quit
```

Each agent line shows herdr's status (`◐` working, `◉` blocked, `✳` done, `○` idle), the Claude profile, context use, tool calls per minute over the last ten minutes (on a tree pane at least 100 columns wide), the session title and, for working agents, the tool running now. Agents are grouped by the main checkout of their working directory. Agents that are crew workers show their board item from `.claude/crew/board.tsv` instead of the title.

Motion follows real activity. A working agent spins while its transcript was written in the last minute, or it waits on a tool call and was written in the last ten minutes, so one that says working but has gone quiet holds still. While an agent that is not blocked has a running subagent or workflow, its first connector column turns cyan and a bright cell runs down it. `··•` steps along a tool line while that call is still out, and a row flashes when it starts a new call. The `live` strip at the bottom (on terminals at least 30 rows tall) lists every agent's, running subagent's and workflow agent's calls, newest first. Frames run at 8 per second only while something moves, so an idle view draws nothing.

## Install

```sh
herdr plugin install Anyesh/godview --ref <commit>
```

The install runs `cargo build --release --locked`, so it needs a Rust toolchain and takes a minute or so. Bind the open action in your herdr config:

```toml
[[keys.command]]
key = "prefix+alt+o"
type = "plugin_action"
command = "godview.open"
description = "godview: every agent and its subagents"
```

`godview.open` focuses the running godview pane if there is one, and otherwise opens it in a new tab, so there is only ever one instance.

## Keys

| key | action |
| --- | --- |
| `j` `k` / arrows | move |
| `enter` | focus the selected agent's pane |
| `p` | show the selected pane's recent output instead of its tool list |
| `w` | show working and blocked agents only |
| `a` | also show finished and stale subagents and workflows |
| `space` | fold or unfold a repo, an agent's children, or the "more done" row |
| `m` | turn motion off or on; sparklines, flashes and the live strip stay |
| `q` | quit |

## Where the data comes from

- **herdr's socket** for agents, statuses, titles, context use and session ids. godview takes a snapshot on every pane lifecycle or status event and every 10 seconds, since title and session changes raise no event.
- **Claude Code transcripts** under `~/.claude/projects` for the current tool and last text, read from a stored offset (at most the last 256 KB).
- **Subagent and workflow files** under `<session>/subagents/` and `<session>/workflows/`, watched with inotify. A subagent with no `finished` mark counts as running only while its transcript changed in the last 90 seconds, or it waits on a tool call and changed in the last 10 minutes.
- **The pane's process environment** for the profile: `CLAUDE_PROFILE`, else the `<name>` of a `CLAUDE_CONFIG_DIR` ending in `.claude-<name>`, else `default`.

Warnings go to `godview.log` in the plugin's state dir, since the screen belongs to the TUI.

## Development

`herdr plugin link` skips the build step, so build first:

```sh
cargo build --release
herdr plugin link .
cargo test
```

Linux only, since it reads `/proc` and uses inotify.

## License

MIT
