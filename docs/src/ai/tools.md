---
title: AI Agent Tools - Zed
description: Built-in tools for Zed's AI agent including file editing, code search, terminal commands, web search, skills, and diagnostics.
---

# Tools

Zed's built-in agent has access to these tools for reading, searching, and editing your codebase. These tools are used in the [Agent Panel](./agent-panel.md) during conversations with AI agents.

The exact tool list can vary by [Agent Profile](./agent-profiles.md), selected model provider, and Zed version.

You can configure permissions for tool actions, including situations where they are automatically approved, automatically denied, or require your confirmation on a case-by-case basis. See [Tool Permissions](./tool-permissions.md) for the list of permission-gated tools and details.

To add custom tools beyond these built-in ones, see [MCP servers](./mcp.md).

To choose which built-in tools and MCP tools are available in a Zed Agent thread, use [Agent Profiles](./agent-profiles.md). Profiles control tool availability; tool permissions control allow, deny, and confirm behavior.

The terminal tool can also run with additional OS-level restrictions when [Zed Agent sandboxing](./sandboxing.md) is enabled.

## Read & Search Tools

### `diagnostics`

Gets errors and warnings for either a specific file or the entire project, useful after making edits to determine if further changes are needed.
When a path is provided, shows all diagnostics for that specific file.
When no path is provided, shows a summary of error and warning counts for all files in the project.

**Example:** After editing `src/parser.rs`, call `diagnostics` with that path to check for type errors immediately. After a larger refactor touching many files, call it without a path to see a project-wide count of errors before deciding what to fix next.

### `fetch`

Fetches a URL and returns the content as Markdown. Useful for providing docs as context.

`fetch` is governed by tool permissions, agent profiles, and project trust. It is not run inside the terminal OS sandbox, but when sandboxing is enabled it uses the same per-host network grants as terminal commands and re-authorizes every redirect target.

**Example:** Fetching a library's changelog page to check whether a breaking API change was introduced in a recent version before writing integration code.

### `find_path`

Quickly finds files by matching glob patterns (like "\*_/_.js"), returning matching file paths alphabetically.

### `git_status`, `git_diff`, `git_branches`, `git_remotes`, and `git_show`

Inspect repository state through Zed's Git backend without invoking a shell. These tools cover working-tree status, staged/worktree/merge-base diffs, local and remote branches, remote URLs, and commit metadata with changed-file names. They are available in Plan and Architect modes because their API surface contains no Git mutation operation.

### `grep`

Searches file contents across the project using regular expressions, preferred for finding symbols in code without knowing exact file paths.

**Example:** To find every call site of a function before renaming it, search for `parse_config\(` — the regex matches the function name followed by an opening parenthesis, filtering out comments or variable names that happen to contain the string.

### `list_directory`

Lists files and directories in a given path, providing an overview of filesystem contents.

### `read_file`

Reads the content of a specified file in the project, allowing access to file contents.

## Web Tools

### `pull_request`

Inspects a public `github.com` pull request or `gitlab.com` merge request using strict HTTPS GET requests. The summary is always returned; files, conversation, reviews or approvals, and checks or pipelines can be requested as additional sections. Private repositories require `GITHUB_TOKEN` or `GITLAB_TOKEN` in Zed's environment. Self-hosted forges are rejected rather than contacted through a guessed API endpoint.

### `search_web`

Searches the web for information, providing results with snippets and links from relevant web pages, useful for accessing real-time information.

**Example:** Looking up whether a known bug in a dependency has been patched in a recent release, or finding the current API signature for a third-party library when the local docs are out of date.

> **Note:** The built-in `search_web` tool is only available to [Zed Pro](https://zed.dev/pricing) subscribers using the Zed provider. If you're on a free plan or using a different provider, you can get equivalent functionality by connecting an MCP server that provides web search capabilities. See [MCP servers](./mcp.md) for details.

## Edit Tools

### `copy_path`

Copies a file or directory recursively in the project, more efficient than manually reading and writing files when duplicating content.

### `create_directory`

Creates a new directory at the specified path within the project, creating all necessary parent directories (similar to `mkdir -p`).

### `delete_path`

Deletes a file or directory (including contents recursively) at the specified path and confirms the deletion.

### `edit_file`

Edits files by replacing specific text with new content.

**Example:** Updating a function signature — the agent identifies the exact lines to replace and provides the updated version, leaving the surrounding code untouched. For widespread renames, it pairs this with `grep` to find every occurrence first.

### `move_path`

Moves or renames a file or directory in the project, performing a rename if only the filename differs.

### `write_file`

Creates a new file or overwrites an existing file with completely new contents.

### `terminal`

Executes shell commands, creating a new shell process for each invocation. Fast commands return their final output. Commands still running after **10 seconds** return a task ID so the agent can continue independent work while the same process runs in the background. Set `yield_ms` between 0 and 30000 to change that foreground wait; zero yields immediately. `timeout_ms` remains a separate hard runtime limit and kills the process if reached, even after it has moved to the background.

The lifecycle tools below follow the profile's existing `terminal` switch; they need no separate configuration. Tasks belong to their conversation, survive subsequent prompts and compaction, and are canceled by explicit Stop or thread teardown. They are not restored after restarting the app. Completion updates the terminal UI but does not automatically start a new model turn.

At most eight tasks can run per conversation, with up to 32 task records retained. Background-capable commands cap captured output at 100 KiB and selected model output at 16 KiB. Head/tail selection operates on the captured output, so truncated captures may not include the command's final lines.

**Example:** Start a test command with a hard runtime limit and `tail_lines: 30`, then inspect unrelated code while it runs. When no independent work remains, call `terminal_wait` using the returned task ID and check the final output before claiming the tests passed.

### `terminal_status`

Returns a task's current bounded output, whether it is still running, and its exit status when available. This is a snapshot, not a wait; the agent should not repeatedly poll it while idle.

### `terminal_wait`

Waits for a task to finish without busy-polling. It returns immediately when the process exits, or reports that it is still running after the wait limit (30 seconds by default, configurable from 1 to 60000 milliseconds). **The wait limit does not kill the process.** This is the tool to use when the agent has nothing useful left to do except wait.

### `terminal_stop`

Stops an existing task in the same conversation and reports its output/status. It cannot execute a command or stop arbitrary system processes. Like terminal execution, it is unavailable in Plan, Architect, and restricted-workspace mode; status and waiting are read-only.

## Other Tools

### `ask_question`

Asks for a preference or missing requirement, with free text, a single choice, or multiple choices. When the agent supplies a valid recommendation, the question shows it with a **10-second countdown**. If you do not interact, Praxis uses that recommendation and continues; the conversation records this as an automatic recommendation, not as your answer.

Typing, changing a selection, or submitting pauses automatic continuation for that question. Decline, Cancel, and stopping the agent never select the recommendation. Questions without a safe recommendation wait for a manual answer. This timeout applies only to `ask_question`, not to tool permissions or approval to leave Plan mode.

### `skill`

Loads instructions from an available [Skill](./skills.md) so the agent can follow project-specific or workflow-specific guidance. Skills can also be invoked by you directly with slash commands.

**Example:** When a repository has a skill for release-note writing, the agent can load that skill before drafting release notes so it follows the local format.

### `spawn_agent`

Spawns a subagent with its own context window to perform a delegated task. Useful for running parallel investigations, completing self-contained tasks, or performing research where only the outcome matters. Each subagent has access to the same tools as the parent agent.

**Example:** While refactoring the authentication module, spawn a subagent to investigate how session tokens are validated elsewhere in the codebase. The parent agent continues its work and reviews the subagent's findings when it completes — keeping both context windows focused on a single task.
