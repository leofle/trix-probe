# trix-probe

Lets an LLM use [trix](https://github.com/leofle/trix) the way a person does (keyboard, mouse, a window that resizes and closes) to find bugs and UX problems.

trix-probe runs trix in a virtual terminal inside a throwaway sandbox. It turns the screen into text the model can read and a PNG in real colors it can look at. It also records what a person would notice: agents changing state, desktop notifications, sounds, bells, clipboard copies, the window title, errors in the server log. Every agent in the sandbox is a scriptable fake that fires trix's real hooks, so the model can make any agent need you, finish, crash or flood its pane on cue, without spending tokens on real agents.

It works two ways:

- **As an MCP server.** Claude Code (or any MCP client) gets 26 tools (`launch`, `press`, `type_text`, `click`, `resize`, `batch`, `sweep`, `compare`, `fake_agent`, `expect`, `report_issue`, …) and you steer the testing yourself.
- **On its own.** `trix-probe explore` hands the tools to Claude through the API. Claude explores trix, checks it against the docs and writes a report. Each issue comes with a screenshot and a replayable repro.

```
 Claude ──tools──▶ trix-probe ──keys, mouse, resize──▶ PTY ──▶ trix (client) ──▶ trix server
   ▲                  │                                                            │
   └── screen as text │◀── alacritty_terminal (the VT engine trix uses)            ├─▶ fake claude ─┐
       + PNG          │◀── trix events --json, notify/sound/clipboard loggers      ├─▶ fake codex   │ trix's real
       + what changed │                                                            └─▶ shell        │ hooks
                      └─ /tmp/tp-xxxxxx: own HOME, XDG dirs, sockets, demo git repo ◀───────────────┘
```

## Install

Needs Rust and a `trix` to test. macOS or Linux.

```sh
git clone https://github.com/leofle/trix-probe.git && cd trix-probe
cargo install --path .
trix-probe doctor        # checks trix, fonts, git, and that trix starts in a sandbox
```

`--trix PATH` picks the trix binary to test (default: `$TRIX_PROBE_TRIX`, then `trix` on your PATH). To test your working copy, point it at `~/Projects/trix/target/release/trix`.

## With Claude Code (MCP)

```sh
claude mcp add trix-probe -- trix-probe mcp --trix "$(which trix)"
```

Then ask, for example:

> Use trix-probe to test trix's settings screen (alt+,): every row, at 140x40 and at 80x24, with dark and light terminal colors. Check what it does against trix's README. Report each problem with report_issue.

> With trix-probe, open three fake claude agents, make the one I'm not looking at ask for permission, and check the needs-you strip, alt+a, the notification and the sound. Then do it again while the window is unfocused.

Each call returns the screen: whole at first, after a resize and every eighth look; otherwise only the rows whose text changed since the model last saw it (the clock, timers and spinners ticking don't count), plus a note of rows whose colors alone changed. Runs of identical rows are folded into one line. `screen` and `screenshot: "full"` always show it all. Actions wait for the screen to settle the same way: a working agent's spinner doesn't hold them up. Every action is logged, so `report_issue` saves the steps that led to the bug. `batch` runs several actions in one call and looks once at the end, keeping what each step reported; its steps are logged one by one.

### Testing a branch

`launch` takes `trix`, the binary to run in that session, so one MCP server can test any build. The result names the binary and its version, and reports and repros record it:

> Build my trix branch, then with trix-probe launch `~/Projects/trix/target/release/trix` and check every dialog at 80x24, 80x8 and 80x6.

`sweep` looks at each case at each size in one call: it resizes, presses the case's keys, looks, and presses esc. It returns every screen, and a contact sheet per case:

```json
{"sizes": ["80x24", "80x8", "80x6"],
 "cases": [{"name": "broadcast", "keys": ["alt+b"]}, {"name": "keys", "keys": ["alt+?"]}]}
```

`resize_while_open: true` opens each case once and resizes it through the sizes instead.

Every screen is checked for layout problems a person sees at a glance: text drawn over a box's border (trix sets its own labels in a border apart with spaces, so text right against the line was drawn over it: `╰──› hello all───╯`), and a dialog drawing outside its box (text that changed outside it when it opened; the bottom bar, which shows the dialog's keys, aside). The result lists them per screen and sums them up; `check_layout: true` makes the sweep fail when there are any, for regressions. Other tools flag text over a border too, when it appears or goes away.

`compare` runs the same steps on two binaries, one after the other in matching sandboxes, and shows the lines that differ, step by step and, inside a sweep, screen by screen. It masks the clock, durations, spinners, sparklines and the fake agents' ids, so only real differences show. Before/after images of each differing screen are saved. With no `steps`, it replays what your session has done since its launch:

> Compare the Homebrew trix with my build on what we just did.

```
── step 2/6 sweep …
  screen · broadcast @ 80x6: 4 of 7 lines differ
    1 before│    │                                                                      │1 ╮
    1  after│    │  › message to send…                                                  │1 ╮
```

## On its own

```sh
export ANTHROPIC_API_KEY=…          # or `ant auth login`
trix-probe explore --docs ~/Projects/trix/README.md
trix-probe explore --docs ~/Projects/trix/README.md --focus "search (alt+f)" --focus "small windows" --max-steps 60
```

| flag | default | |
| --- | --- | --- |
| `--docs` | `trix --help` | trix's README, so the tester can check behavior against the documentation. Strongly recommended. |
| `--focus` | explore broadly | what to concentrate on; repeat for several |
| `--max-steps` | 120 | the most tool calls it may make |
| `--max-usd` | 5 | stop, after a last chance to report, once the estimated cost passes this |
| `--model` | `claude-opus-5-5` | |
| `--effort` | `high` | `low` … `max` |
| `--no-fallbacks` | | don't ask the API to retry a declined turn on a fallback model |

The runner uses adaptive thinking, prompt caching, and the API's context editing (`clear_tool_uses_20250919`), which clears old screenshots once a session grows past about 120k tokens. Reports are kept. The conversation is never edited client-side, so caching and thinking stay valid. Server-side refusal fallbacks are on by default (`fallbacks: "default"`). Cost depends mostly on the number of steps. `--max-usd` caps it, and the estimate is printed at the end and written to the report. Start with a small `--max-steps` to see what a run costs you.

## What you get

Everything lands in `~/.local/share/trix-probe/runs/<time>-<mode>/` (or `--out`):

```
report.md                 every issue, most severe first, plus the tester's summary
issues.json
issues/001-<title>/
  issue.md                steps, expected, actual, severity, confidence
  screen.png  screen.txt  the screen when it was reported
  repro.json              every action from launch to the report
screens/                  each screenshot the tester looked at (explore)
transcript.jsonl          the API conversation, without image data (explore)
```

Replay a repro to confirm a bug, or to check a fix:

```sh
trix-probe replay ~/.local/share/trix-probe/runs/…/issues/001-…/repro.json --trix ./target/release/trix
```

It prints each step and the final screen, and saves a PNG of it. The binary you give wins over one a recorded `launch` names. `--recorded-trix` runs the recorded ones instead.

`replay` exits 1 if any step fails: an action errors, a `wait_for` times out, or an `expect` doesn't hold. `expect` checks the screen, or one row of it, for text or a regex (`absent: true` checks it isn't there), and shows the screen when it fails. Add `expect` steps to a repro and it becomes a regression test:

```json
{"tool": "expect", "args": {"row": 1, "regex": "○ claude .*○ claude-2 ", "why": "pane titles keep the name at 80x24"}}
```

The same comparison from the command line, on a repro or any file of that shape:

```sh
trix-probe compare --before "$(which trix)" --after ./target/release/trix steps.json
```

For a quick look by hand:

```sh
trix-probe try --keys alt+n -o form.png -- claude shell     # launch, press alt+n, print the screen, save a PNG
```

## The fake agents

In the sandbox, `claude`, `codex`, `grok`, `opencode`, `cursor-agent`, `copilot` and `muse` are all `trix-probe fake-agent`. The fake claude reads the `--settings` overlay trix hands it and runs trix's own hook commands with Claude-shaped payloads (`SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PermissionRequest`, `Notification`, `PostToolUse`, `Stop`, `SessionEnd`). It also writes a conversation log with token usage for the context gauge. The fake codex reads the `-c notify=…` and `-c hooks=…` trix hands Codex, and does what Codex does with them: it runs `notify` after every finished turn (`agent-turn-complete`, with the last reply), asks for approval in Codex's words, and runs the lifecycle hooks (Codex-shaped: `exec_command`, `apply_patch`, `Interrupt`) only once `/hooks` has trusted them, since Codex runs no hook you haven't trusted. The other kinds fire no hooks, so trix has to read their screens, as it does for agents without hooks. Every fake takes an answered dialog off the screen, as Claude Code and Codex do.

Type a prompt into a fake agent's pane. Words in it decide the turn:

| word | the agent… |
| --- | --- |
| `permission` | asks to run `rm -rf dist` and waits for 1/2/3 (needs-you) |
| `question` | asks which database to use (needs-you) |
| `long` | works for about 30 s, one tool every 5 s |
| `flood` | prints 3000 lines fast |
| `unicode` | prints wide, combining, RTL, emoji, colored and hyperlinked text |
| `silent` | takes a turn without firing any hook |
| `context 85` | sets the context gauge to 85% |
| `title <text>`, `bell` | sets the terminal title, rings the bell |
| `crash`, `exit` | exits with code 1, or cleanly |
| `/hooks` | (codex) trusts trix's hooks, as Codex's `/hooks` screen does: they run from then on |
| anything else | a short turn: Read, Grep, Edit (a real one-line edit in the demo repo), Bash |

Esc interrupts a turn. Ctrl-C twice exits. The `fake_agent` tool does all of this from outside, by name or number, for agents you aren't looking at. It can also answer a dialog, fire one raw hook, or set the context gauge.

## The sandbox

Each `launch` creates `/tmp/tp-<id>`. The path is short because macOS caps unix socket paths at 104 bytes. Each sandbox has:

- its own `HOME` and `XDG_CONFIG_HOME` / `XDG_STATE_HOME` / `XDG_RUNTIME_DIR`, so its trix server, sockets, config, saved workspace and layouts never meet yours;
- a demo git repo (`work/demo`) with one commit and an uncommitted change, so branch and diff have something to show, plus a second folder (`work/other`);
- shims on `PATH` for the fake agents, and loggers in place of `afplay`, `pw-play`, `notify-send`, `osascript`, `pbcopy` and `wl-copy`. Notifications go through config.toml's `notify.command` to a logger, and `TRIX_NO_NOTIFY=1` keeps them off your desktop.

The probe clears `TRIX_*`, `TMUX`, terminal-identity and Claude Code variables from its own environment first, so it works from inside a trix pane. The sandbox is removed on `teardown`, on the next `launch`, when the MCP client disconnects, and on SIGTERM, SIGINT or SIGHUP. If a probe is killed hard, the next one to start cleans up its sandbox. `--keep` leaves sandboxes in place to inspect.

`launch(agents: "real")` uses the real agents on your PATH, with your real `HOME` so they're logged in. trix's own files still stay in the sandbox. This costs tokens and uses your accounts. An agent that isn't installed is blocked from installing itself.

## The virtual terminal

- **Emulation**: `alacritty_terminal` 0.26, the engine trix uses. It answers device attributes, cursor position, OSC 10/11/4 color queries, and text-area-size requests.
- **Keys**: xterm legacy by default (alt sends an ESC prefix, as in Terminal.app and iTerm2). `keyboard: "kitty"` approximates the kitty protocol's "disambiguate" level for when trix turns it on. Specs look like `alt+n`, `ctrl+down`, `shift+pgup`, `alt+,`, `alt+?`, `f5`.
- **Mouse**: SGR or X10, whichever trix asked for. Click, double-click, right-click, drag and wheel all work, by coordinates or by on-screen text.
- **Focus**: focus-in/out reports (`set_focus`), to test what trix does while unfocused.
- **Colors**: `terminal_colors` sets a dark (Tomorrow Night) or light (Tomorrow) outer terminal.
- **Screenshots**: drawn with a system monospace font (Menlo on macOS; DejaVu Sans Mono or Noto on Linux; `TRIX_PROBE_FONT` overrides). Box drawing, block elements, braille and Powerline caps are drawn by hand. Color emoji come out as outlined boxes, so the text screen is the authority on exact characters.

## Development

```sh
cargo test        # unit tests, an end-to-end needs-you round trip, and the regression suite, against the trix on PATH (skipped without one)
TRIX_PROBE_TRIX=~/Projects/trix/target/release/trix cargo test --test regressions    # the regressions against a build
```

The regression suite replays every `tests/regressions/*.json` (`{"title", "notes", "actions"}`, the shape of a repro) one at a time and fails if any step fails. `TRIX_PROBE_ONLY=grid` runs just the files whose names contain `grid`. To add one, reproduce the bug with trix-probe, add `expect` steps for what should be on screen, and check it fails on a build with the bug and passes on one without.

When the fix isn't in a trix release yet, give the file `"fixed_in": "1.0.8"` (the first release with it). It's skipped against older trix, so testing the latest release stays green. A build from source still reports the last release's version, so set `TRIX_PROBE_UNRELEASED=1` to run those files against `main` or a branch; CI does this for its source builds.

```
src/main.rs     CLI: mcp, explore, replay, compare, try, doctor (+ the internal fake-agent and log-cmd)
src/probe.rs    a testing session: sandbox, terminals, actions, observations, issues and reports
src/tools.rs    the tool schemas and dispatch, shared by MCP and explore
src/compare.rs  the same steps on two binaries, masked and diffed
src/mcp.rs      MCP over stdio (JSON-RPC 2.0)
src/explore.rs  the autonomous runner (Claude Messages API over HTTP)
src/term.rs     PTY + alacritty_terminal, snapshots, the outer terminal's palette
src/keys.rs     key names → bytes, mouse and focus encoding
src/render.rs   snapshot → PNG
src/fake.rs     the fake agent
src/sandbox.rs  the sandbox: folders, env, shims, demo repo, teardown
src/cleanup.rs  teardown on signals
```

## License

MIT
