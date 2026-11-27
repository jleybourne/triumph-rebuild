# agents/secondary-agents-updates.md — status of the secondary agents, for the lead

Written and kept current by the session that started the helper agents, for the lead to read at a glance.
Not committed by the helpers: this file sits only in the lead's tree, so commit or delete it as you like.
Each lane's own detail is in its `agents/<lane>.md` on its branch, and arrives with its pull request.

**Last updated:** 2026-11-27 12:45 (Adelaide)

## Lanes

| lane | build | branch | worktree | state | last reported |
|:--|:--|:--|:--|:--|:--|
| **loader** | Build 3, first half (L1–L4) | `agent/loader` | `D:\Developer\triumph-rebuild-loader` | 🔄 started 12:40 | nothing yet |
| **shader** | Build 4 front end (S1–S4) | `agent/shader` | `D:\Developer\triumph-rebuild-shader` | 🔄 started 12:40 | nothing yet |

No pull requests yet. Neither lane merges; both open a PR to `master` titled `agent/<lane>: <what closed>`.

## Rules every lane was given

- `secondary-agent.md`, word for word, plus a reminder of the points below.
- Its own worktree only; never `checkout` or `switch` in `D:\Developer\triumph-rebuild`.
- None of the lead's files in section 2. One line in the root `Cargo.toml` `members` list is allowed.
- A separate check for `agents\MEASURING` before every build or test, and a wait while it exists. No dodging a block with chained short sleeps.
- `cargo build --release -j 4`. No GPU work of any kind.
- No game bytes in git: hashes, names and counts only. The game folder is read-only, through `REBUILD_GAME_DIR`.
- Licences: xcl (GPL-2.0) for design only. Only non-performance code from the fork, with the header and a `CREDITS.md` line. The shader IR, lowering and emission are the lead's.
- Every task ends in a test or a number.

## For the lead

Nothing yet. Anything a lane flags about the lead's code goes here as a one-line pointer to its `agents/<lane>.md`, and is never fixed by the lane.

## Log

| time | event |
|:--|:--|
| 12:40 | Both lanes started. The old project's (fxceless-victory) recurring status, sync, continuation and fork-check jobs were cancelled; no further work on it. |
