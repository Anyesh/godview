# godview

A [herdr](https://herdr.dev) plugin pane that shows every agent in your herdr session as one live tree, with each Claude Code agent's subagents and workflows nested under it.

```
 godview   18 agents · 5 working · 1 subagent running                                  w working only · a all
┌ agents ──────────────────────────────────────────────────────────────┐┌ worker-3 · beta · incr ────────────┐
│▶ ▾ incr                                                              ││Integrator handoff                  │
│    ◐ worker-1     alpha     31%  rule A natural walk                 ││context 44% (438k) · cache hit 99.7%│
│                                  Bash  cargo nextest run -p s…   12s ││                                    │
│    ◐ worker-3     beta      44%  Integrator handoff                  ││recent tools                        │
│                                  Agent  Review T10 diff          20s ││  20s  Agent  Review T10 diff       │
│      ├ ● code-reviewer   Review T10 scan-two diff   Read         20s ││   4s  Read   src/sched/lib.rs      │
│      └ ◆ workflow review-changes   Verify 3/5 · 7 agents          1m ││                                    │
│    ✳ integrator   beta      32%  Multi-agent handoff              2m ││last text                           │
│    ✳ 9 more done                                                 [+] ││ Audit subagent is still running.   │
│  ▾ harness                                                           ││                                    │
│    ○ harness      alpha      9%  Herdr god view                   8m ││                                    │
└──────────────────────────────────────────────────────────────────────┘└────────────────────────────────────┘
 j/k move · enter go to pane · p peek output · space fold/expand · q quit
```

Each agent line shows herdr's status (`◐` working, `◉` blocked, `✳` done, `○` idle), the Claude profile, context use, the session title and, for working agents, the tool running now. Agents are grouped by the main checkout of their working directory. Agents that are crew workers show their board item from `.claude/crew/board.tsv` instead of the title.

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
