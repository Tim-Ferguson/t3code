# Browser fixture

Starts an isolated `t3-server` in a rich, realistic state for manual and browser checking, using only fakes: a dirty
repository, a GitHub-looking and a GitLab-looking project with pull requests, and about thirty seeded conversations
(approvals, questions, queued messages, a fork, subagents, background work, usage limits, a 36-turn thread, and more).
Nothing leaves the scratch directory or the loopback interface.

## Run it

Build once (the tool builds nothing itself):

```sh
cd rust
CARGO_TARGET_DIR=<target> cargo build -p t3-server -j 4           # <target>/debug/t3-server
# the web UI: the coordinator builds it with dx; its output directory is <...>/web/public
```

Then, with a port (47840-47849 for agent runs) and a scratch directory that does not exist yet or is empty:

```sh
S=/private/tmp/claude-501/t3code-rust-port-test-fixture-$RANDOM
rust/tools/browser_fixture/start.sh --port 47841 --scratch "$S" \
  --server <target>/debug/t3-server --assets <...>/web/public
```

It prints the URL, a pairing credential (one use, five minutes), the PID, and how to stop. Then, without a browser:

```sh
node rust/tools/browser_fixture/check.mjs http://127.0.0.1:47841 "$(rust/tools/browser_fixture/pair.sh --scratch "$S" --server <target>/debug/t3-server)"
```

With a headless Chromium of Playwright's own (never the machine's Chrome), `browser_check.mjs` pairs through the link and drives the
UI: the right panel's launcher or add menu (Browser, Device, Terminal), a Terminal, a Device and a Browser tab, the Integrations page
(browser zoom, appearance, link target, profiles kept across a reload, the question before a profile's data is cleared) and the
Diagnostics page's resource history. Start the fixture with `--server-browser` (and `--hook` for the pull request reads of `check.mjs`) so
the Browser row is offered; there is no Chrome to start in the fixture, so the tab opens with its address bar and empty state and its
stream fails to start (the server logs "The server preview browser could not start", the page retries with the viewer's backoff):

```sh
node rust/tools/browser_fixture/browser_check.mjs http://127.0.0.1:47841 "$(rust/tools/browser_fixture/pair.sh --scratch "$S" --server <target>/debug/t3-server)" \
  --playwright <node_modules>/.pnpm/playwright-core@1.60.0/node_modules/playwright-core
```

Stop it (checks that the PID is still the process that was started: its executable and working directory; never by
name or pattern; leaves `$S` for inspection):

```sh
rust/tools/browser_fixture/stop.sh --scratch "$S"      # or: kill "$(cat "$S/server.pid")"
```

`stop.sh` also stops the fake GitHub API process (`$S/github-api.pid`); `kill` of the server alone does not.

Options: `--reuse` (use a directory that has content; restarts over an earlier run's projects and history, but live
states, such as pending approvals and the held run, do not survive a restart), `--no-seed`, `--no-github-api`,
`--no-sandbox`, `--hook`, `--with-grok`. `--scratch` must be absolute, must be named `t3code-rust-port-test-...`, and is
refused under `~/.t3`, under the dotfiles of the real home directory, or if it is or contains the home directory.

## What is created (all under the scratch directory)

| Path                                                               | What                                                                                                                            |
| ------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------- |
| `state/`                                                           | the server's state directory (`--state-dir`)                                                                                    |
| `home/`, `providers/*`                                             | the fake `HOME` and each provider's home; the real home is never visible                                                        |
| `bin/`                                                             | the server's whole `PATH` apart from `/usr/bin:/bin` (for git, sh): fake `codex`, `claude`, `glab`, `gh`, `open`, and `python3` |
| `settings.json`                                                    | Codex and Claude on the fakes, text generation on the fake Codex, every other provider disabled, update checks off              |
| `projects/`                                                        | the four git projects below (plus `commitcheck`); this is also the server's working directory                                   |
| `remotes/`                                                         | local bare repositories that the GitHub and GitLab projects push to                                                             |
| `logs/fake-calls.jsonl`                                            | one line per call of any fake (and per refused `open`): proof of what ran                                                       |
| `server.log`, `server.pid`, `meta.txt`, `server.sb`, `seeded.json` | log, PID, what `stop.sh` checks, the sandbox profile, the seeded thread ids                                                     |

Every project is named `t3code-rust-port-test-...` and its README says "Created by the T3 Code Rust port browser fixture. Safe to delete."

| Project          | For                                                                                                                                                                                                                                                                                       |
| ---------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `-dirty`         | the commit dialog and diff panel: modified (hunks), staged-then-edited (`MM`), staged add, staged rename with an edit, deleted, a changed binary, a new binary, an untracked note, a 1.3 MB untracked log, and a tracked CSV whose diff is far past the review panel's 120 KB patch limit |
| `-github`        | the GitHub pull request panel. `origin` is `https://github.t3code-test.invalid/acme/widgets.git` (the server reads the host from `git remote -v`); pushes go to `remotes/widgets.git` through `pushInsteadOf`. Branch `feature/pr-panel-demo`, four commits                               |
| `-gitlab`        | the GitLab merge request panel, same shape: `https://gitlab.t3code-test.invalid/acme/lab.git`, branch `feature/mr-panel-demo`                                                                                                                                                             |
| `-conversations` | where most seeded threads run; the tree stays clean                                                                                                                                                                                                                                       |
| `-commitcheck`   | `check.mjs` writes a file here and commits it through `git.runStackedAction`, to prove commit messages can be written                                                                                                                                                                     |
| `-ship`          | the git actions: a second clone of the GitHub project's repository (same host, same bare remote) on `feature/ship-demo`, one commit not pushed and two uncommitted edits, so push, commit and push, commit on a new branch and create a pull request each have something to do            |

## The fakes

- `fakes/fake_codex.py`: `codex exec` (the one-shot run the server makes to write commit messages, pull request
  title and body, branch names and thread titles) is answered from the prompt and the output schema, with no model. Everything
  else is handed to the repository's scripted app-server, `crates/server/tests/fixtures/codex-provider.py`. Prompt tags
  choose the behaviour: `[approval]`, `[question]`, `[hold]` (keep the run open), `[file]`, `[plan]`, `[todo]`,
  `[think]`, `[background]`, `[persistent-tool]`, `[mcp-tool]`, `[retry]`, `[usage-limit]`. Typing them in the UI works too.
- `fakes/fake_claude.py`: runs `claude-provider.py` unchanged but picks its scenario from a tag in each prompt:
  `[subagent]`, `[children]`, `[background]`, `[wake]`, `[limit]`, `[warning]`, `[approval]`, `[question]`, `[hang]`,
  `[steer]`, `[error]`, `[crash]`; no tag: thinking, a Bash call, a todo list, a reply.
- `fakes/fake_glab.py`: GitLab merge requests for `acme/lab` (7 open with a failed pipeline, two reviewers, labels, two
  review threads one resolved, comments with awards, four commits, four files; 8 draft; 9 merged; 10 closed). Writes the
  server makes (notes, replies, resolutions, awards, edits, approvals, reviewers, merge, close, reopen, draft, rebase) are
  applied to `logs/glab-state.json`, since every call is a new process; a fresh scratch directory starts again. An unknown
  call exits 97 and names its arguments on stderr, so a gap shows in the UI instead of as an empty panel.
- `fakes/fake_gh.py`: only `--version`, `auth status --json hosts` and `auth token` for the fixture host. The server reads GitHub over HTTP, not through `gh`.
- `fakes/fake_github_api.py`: the GitHub API (REST and GraphQL) for `acme/widgets`: #42 open with checks in every
  state (success, failure, in progress, skipped, neutral, queued, cancelled, two commit statuses), three review threads
  (open, resolved, outdated), comments and reviews with reactions, labels, reviewers including a team, four commits, four
  changed files; #43 open and mergeable (every check passed, approved: the panel offers Merge); #41 draft; #40 merged; #39 closed. The writes the server makes (comments, replies, resolutions, reactions,
  edits, reviews, reviewer requests, labels, viewed marks, merge, auto-merge, update branch, close, reopen, draft) change this
  process's memory and show in the next read; a restart starts again. It recognises GraphQL documents by reading
  `crates/server/src/pull_request_queries.rs`. **It is idle unless the server was started with `--hook` (see Server hooks).**
- `fakes/fake_refuse.py`, installed as `open`: records and refuses.

## Seeded conversations

Each title says what it demonstrates; `seeded.json` lists the ids. Finished tool call and reply; plan card, todo list and
reasoning; pending command approval; pending question; pending file-change approval; a run held open with two queued
messages; a fork with its parent; pinned, snoozed and settled threads; Codex background command and a persistent tool;
MCP and dynamic tool calls; a reconnecting banner then a reply; a usage-limit stop, and another with automatic recovery
armed (the scripted Codex stamps its turns at 2027-01-15 08:00 UTC and reports a reset an hour later, so the recovery
is snoozed until then); a 36-turn thread; a thread in the dirty project; a thread linked to GitHub #42 and one linked to GitLab !7; Claude: a
normal turn, a subagent with a child thread and a pending approval, a background command, a failed run (HTTP 429), a
usage-limit stop, a pending question.

## What keeps it contained

- The server runs with an explicit environment, not the caller's: `HOME` and `TMPDIR` in the scratch directory, `PATH`
  is `<scratch>/bin:/usr/bin:/bin`, git has no global config and may use only `file://` (`GIT_ALLOW_PROTOCOL=file`),
  every `T3CODE_*` network base (`BITBUCKET_API_BASE_URL`, `USAGE_RATES_URL`, `CLAUDE_API_BASE`, `OPENCODE_GO_USAGE_URL`) points at
  `127.0.0.1:1`, and `HTTP(S)_PROXY`/`ALL_PROXY` point there too, so any other request the server's HTTP client makes fails at once.
- On macOS the server and the fake API are also started under `sandbox-exec` with a profile (`server.sb`) that refuses
  every network connection except to this machine and refuses to start anything under `/Applications` except Xcode (the
  `git` on the PATH is Xcode's). `sandbox-exec` execs the program, so the PID is the server's. This is what keeps "Open
  in editor" from starting VS Code: the server's editor probe scans `/Applications` and no setting changes that (see
  the hook list). `--no-sandbox` removes it; on Linux there is none.
- The server's PID is the process that was started (no shell wraps it); `stop.sh` checks its command and working directory first.
- Nothing is committed, pushed or changed in this repository.

## Toggles (making a write fail)

A file in `<scratch>/toggles/` makes one fake write fail, so each git action shows both outcomes. Create it to fail, delete it to
succeed; its text, if any, is the message. `push` makes the bare remote's `pre-receive` hook refuse every push; `createPullRequest`
and `createRepository` make the fake GitHub API answer 422 to creating a pull request or a repository; `stackMerge` makes it answer a stack merge (`PUT repos/*/*/pulls/N/merge-async`) with `failed` (without it the layers up to `N` are merged). Without them,
`POST repos/*/*/pulls` opens the next pull request (#44 and on) and `POST user/repos` makes a bare repository under `remotes/` and
answers with its `file://` URL, so "Publish repository" can finish. The fake `gh` answers for `github.com` too, which is where a
repository with no remote is published. Pull requests #42 and #43 are layers 1 and 2 of one stack (`stack { number size }` on the
rows, `GET repos/*/*/stacks`), and the GitHub project's bare remote holds `refs/pull/42/head`, `refs/pull/43/head` and the branch
`feature/smoke-test` (only on the remote), so "Check out" a pull request succeeds: a `git` in `<scratch>/bin` rewrites the
host's URL to the bare repository for `fetch`, `ls-remote` and `pull` only, since rewriting it for `git remote -v` would change the
host the server reads.

## Server hooks (testing only)

- **`--hook`** passes `--fixture` to `t3-server serve` and sets `T3CODE_FIXTURE_GITHUB_API_ORIGIN` to the fake GitHub API's
  loopback origin. `--fixture` is a testing flag: the server then offers no editors (no `/Applications` scan, no file
  manager) and sends GitHub requests to the fake as `{origin}/rest/{host}` and `{origin}/graphql/{host}`, instead of
  `https://api.github.com` or `https://<host>/api/v3`. The server refuses to start if the variable is set without the flag,
  or if it is not `http://127.0.0.1:<port>`, `http://[::1]:<port>` or `http://localhost:<port>` with nothing after the port.
  Without `--hook` the GitHub panel fails with "Could not reach GitHub at github.t3code-test.invalid." (the correct,
  contained failure) and `availableEditors` lists what the machine has; the sandbox above still stops a launch.
- **`--with-grok`** enables the fake Grok (`grok-fake-cli.py` with `grok-turn-agent.py`) and seeds two Grok threads: a tool
  call, and a background command. The adapter holds that run open while the command lives, so the thread is _running_;
  Grok keeps no `pendingBackgroundTasks` roster (Claude and Codex do).

Also seen: `vcs.refreshStatus` reports the staged rename in `-dirty` as two paths, `src/util.ts` and `helpers.ts}`. That is
the original's behaviour, reproduced exactly: its status path reads plain `git diff-index -M --numstat` and keeps what
follows the first `=>`, and the porcelain entry adds the old path. The review and checkpoint diffs read `-z` output and
are right. `crates/vcs/tests/git_rename_parity.rs` records it.

Not faked, and possible without any hook: Azure DevOps (a fake `az` on `PATH`), Forgejo (`fj`/`tea`), Bitbucket (an HTTP server
at `T3CODE_BITBUCKET_API_BASE_URL`, with `T3CODE_BITBUCKET_EMAIL` and `T3CODE_BITBUCKET_API_TOKEN` in the server environment),
the usage rates table (`T3CODE_USAGE_RATES_URL`). OpenCode, Pi and Antigravity have fake CLIs in
`crates/server/tests/fixtures/` that a `binaryPath` could point at; they stay disabled here.
States that need the test suite's manual clocks (idle release, scheduled task runs, watch timers) cannot be reached.
