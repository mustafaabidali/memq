# Use memq

Start with a Git project. memq keeps decisions and progress so your next coding
session can pick up the work.

## Get started

1. Install Git, Rust, and a C/C++ compiler.
   On Ubuntu 22.04, [select GCC 12 first](#build-tools-on-ubuntu-2204).
2. Open the memq source folder and install it:

   ```sh
   cargo install --path . --locked
   ```

3. Open the project you want memq to remember:

   ```sh
   cd /path/to/your/project
   memq init
   memq note --text "Set up memq for this project." --idempotency-key "setup-v1"
   memq brief --compact
   ```

`init` creates `.memq/config.toml`. Keep its generated `project_id`.
If that file already exists, skip `init` and run `brief`.
Notes are saved in `.memq/notes/`. A note tries to stage its own file in Git.
It does not create a commit. Check the returned `durability` and `staging` fields.
Review the config and notes before committing them.
You can save a new note when older sources are missing. A `recovery` field
reports that gap; saving the note does not restore the missing history.
Store validation, deletion rules, and write locks still apply.

Commands return JSON for your agent. Run `memq COMMAND --help` for its options.
Use `--repo /path/to/project` to run a command from another folder.

### Build tools on Ubuntu 22.04

The bundled vector library does not build with Ubuntu 22.04's default GCC 11
or Clang 14. Install GCC 12 and select it before building:

```sh
sudo apt-get update
sudo apt-get install gcc-12 g++-12
export CC=gcc-12 CXX=g++-12
cargo install --path . --locked
```

On macOS, use the C/C++ compiler from the Xcode Command Line Tools.

## Use it during work

| Task | Command |
| --- | --- |
| Pick up work | `memq brief --compact` |
| Focus the briefing | `memq brief --compact --task "login"` |
| Find past context | `memq search "login" --compact` |
| Read the full evidence | `memq show "ITEM_ID" --view-id "VIEW_ID"` |
| Save progress | `memq note --text "What changed" --idempotency-key "unique-step-key"` |

Replace `ITEM_ID` with an item's `id` and `VIEW_ID` with `freshness.view_id`
from a response. The view keeps the evidence tied to the same set of records.
A saved view may describe an older state of the project.
If the briefing used `--branches`, repeat that setting when reading its view.

Use one note key for one piece of work. If a command is interrupted, retry the
same text and key. Use a new key for new work. Add `--evidence "ITEM_ID"` to link
a note to an existing record.

## Keep briefings small

```sh
memq brief --compact --budget 2000
memq search "login" --compact --budget-kind bytes --budget 12000
```

The limit keeps the reply short. It does not delete saved records.
`--compact` keeps record text, source links, approval status, and warnings.
It leaves out detailed source bookkeeping. To read those details, use `show`
with the same item and view IDs and leave out `--compact`.
You can request several IDs in one `show` call.

Compact replies share repeated bodies and source details when that saves space.
If `memq.encoding` is `shared-v1`, `items` holds each body once.
`occurrences` lists ordered `[item, source]` pairs, using indexes that start at zero.
A sourced item inherits `item_defaults`; its own fields override those defaults.
The source supplies `observation` and `availability`.
Join `source.pointer_prefix` and `item.pointer` to get the full source link.
A `null` source marks a plain placeholder, which inherits nothing.
Each reply includes this rule in `encoding_note`.

The default is 4,000 tokens, counted with `o200k_base`.
You can choose `cl100k_base` with `--tokenizer`.
These counts may differ from the token counter used by your model.

Read these fields before relying on a result:

| Field | Meaning |
| --- | --- |
| `freshness.status` | Whether the view is current, stale, or incomplete. |
| `coverage` | Which sources and optional tools were available. |
| `incomplete` | Some context is missing, old, or left out of this reply. |
| `omitted_count` | Records not fully included in this part of the reply. |
| `continuation` | A value to request the next part, when one exists. |

`changes_since.paths` is a small preview of changed file names. When some names
are left out, `paths_count` gives the total and `paths_truncated` is `true`.
The full change list stays in the saved view. Use Git and your code tools to
inspect changes before planning edits; the preview is not a complete file map.
If this is the only omitted content, `reason` is `changes_paths_omitted`.

To continue, repeat the same command and options with
`--continuation "RETURNED_VALUE"`. For `show`, repeat the same item IDs too.
Compact briefings return more evidence on each page.
A split record includes `text_range`, `total_bytes`, and `complete`.
The byte ranges let the agent join the parts without losing text.
If the limit is too small to make progress,
memq returns `budget_below_minimum`; raise the limit and retry.

A search match is evidence to inspect. It does not prove a feature works or
that a decision was approved.

## Add existing project documents

New projects read memq notes. To include a Markdown file, create the file and
append this to `.memq/config.toml`:

```toml
[[source]]
id = "project-guide"
kind = "markdown"
path = "docs/project.md"
```

Run `memq reconcile` after changing the config. Reads also refresh their sources.

These source types are supported:

| `kind` | Extra settings |
| --- | --- |
| `memq-notes` | A folder of notes created by memq. |
| `markdown` | A Markdown file. |
| `json-records` | `collection` names the array; `id_field` names each record's ID. |
| `markdown-table` | `id_column` names the table column holding each record's ID. |

Paths are relative to the Git project. Give each source a stable, unique `id`.
A larger [example config](../tests/fixtures/config/valid-project.toml) shows how
to read several sections of an existing manifest.

Approval rules belong to the source config. For example, a table with `Status`
and `Decider` columns can use:

```toml
[source.policy]
status_field = "Status"
accepted_values = ["approved"]
proposed_values = ["proposed"]
decider_field = "Decider"
approvers = ["maintainer"]
require_attribution = true
```

Put this block directly after the source it applies to.
Match these values to your project's rules. memq does not guess who may approve
a decision. A progress note is not approval.

If a configured source goes missing, memq reports it and keeps the saved evidence.
When you intentionally remove a source from the config, run
`memq reconcile --allow-source-removal` to accept that config change.

## See team changes

Add a remote branch to the config:

```toml
[remote]
name = "origin"
ref = "refs/heads/main"
min_interval_seconds = 300
timeout_seconds = 20
```

memq reads changes from that branch using your existing Git access.
It keeps incoming records separate from your local work.
An approved decision upstream is shown as an upstream decision, not proof that
your local code includes the change.

Briefings include incoming records by default. Search defaults to local records.
Use `memq search "login" --compact --incoming true` to search incoming records too.
Pass a returned `--view-id` to search or show to keep that view's scope.
Use `--incoming false` to leave incoming records out of a new briefing.
Use `--branches main,feature` to include named local branches.

## Connect your coding agent

1. Put `memq` on the agent's `PATH`.
2. Add the [startup instructions](../examples/harness/startup-instructions.md)
   to the project's agent instructions, such as `AGENTS.md`.
3. Copy the matching integration example:

| Agent | Example | Where it goes in your project |
| --- | --- | --- |
| OMP | [Extension](../examples/harness/omp.ts) | `.omp/extensions/memq.ts` |
| OpenCode | [Plugin](../examples/harness/opencode.ts) | `.opencode/plugins/memq.ts` |
| Codex | [Command hooks](../examples/harness/codex-hooks.json) | Merge into the hook config supported by your Codex build. |

The examples target OMP 18.1.18, OpenCode 1.18.30, and Codex 0.154.0.
Check your agent's support for extensions, plugins, or command hooks.
For Codex, start with the project instructions. The hook example also needs
command hooks to be enabled and trusted in Codex.
The instructions tell the agent to run `memq brief --compact` when it starts
or returns from compaction.

OMP and OpenCode can use `MEMQ_BIN` to select a memq binary outside `PATH`.
The agent should save a short note after meaningful work. Hooks do not decide
what was completed.

### MCP

Start the server with:

```sh
memq --repo /absolute/path/to/project mcp
```

In your MCP client, use command `memq` and arguments
`["--repo", "/absolute/path/to/project", "mcp"]`.
The server offers four tools: `brief`, `search`, `show`, and `note`.
They use the same memory as the CLI.
Set `compact: true` on a read tool to shorten its source metadata.

For clients that need text responses, add `--text-fallback` to the arguments.
OpenCode's current client uses this mode.

## Read past agent sessions

Capture is optional. Set the local store paths for the agents you use:

```sh
export MEMQ_OMP_STORE="$HOME/.omp/agent/sessions"
export MEMQ_CODEX_STORE="$HOME/.codex/sessions"
export MEMQ_OPENCODE_STORE="$HOME/.local/share/opencode/opencode.db"
memq capture
```

Change these paths if your stores are elsewhere. Use environment variables to
keep personal paths out of shared config files.
memq reads only sessions it can link to this Git project.
Run `memq doctor --probe-harnesses` to inspect the default store formats.

Unsupported formats are reported in `coverage`. Codex capture needs an explicit
`ordinal` record number in journal entries. Older journals without it are not
supported.
If a session's recorded folder has moved or disappeared, memq cannot prove which
project it belongs to. It reports incomplete coverage and retries later.
Other readable sessions can still be included.

If you have identified an OMP or Codex log you do not want to capture, exclude
its relative file path in the capture settings:

```toml
[capture]
exclude = ["old-folder/old-session.jsonl"]
```

Use a narrow path fragment: it matches any relative log path containing that
text. Excluded files are counted in capture coverage.

## Add semantic search

Exact matches and SQLite FTS5 text search work without an embedding model.
Semantic search can also find records with similar meanings and different words.
It adds its own matches to text search results.

To try it:

1. Install `sentence-transformers` in a separate Python environment.
2. Append this config, using absolute paths to that Python and this example:

   ```toml
   [vectors]
   command = ["/path/to/venv/bin/python", "/path/to/memq/examples/embed-local.py"]
   model = "intfloat/multilingual-e5-small@614241f622f53c4eeff9890bdc4f31cfecc418b3"
   dims = 384
   preprocessing_version = "e5-prefix-v1"
   timeout_seconds = 60
   ```

3. Run `memq embed`, then `memq search "your question"`.

The [example adapter](../examples/embed-local.py) runs the model locally.
Its first run may download the model. This model is an example, not a required
default. The one-shot command reloads it each time; the example also has a
`--serve` mode to keep it loaded.

Run `memq embed` again after records change. If vectors are missing, old, or
unavailable, text search still works and `coverage` shows the gap.
The default search limits are 50 candidates per route and 20 results.
The model choice and ranking settings may change as testing improves.

## Use code tools alongside memq

Use fff to search current files and codebase-memory-mcp to follow code links.
memq does not build a full code index.

An external adapter can pass a code-tool report through `MEMQ_CODE_REPORT`.
memq checks its project, revision, and listed file hashes before using it.
Checks from the current checkout do not establish coverage for another branch
or incoming change.
The [report tests](../tests/code_coverage.rs) show the format.
This is a report input, not an automatic connection to codebase-memory-mcp.

## Save test results

Use `memq note --kind verification --verification result.json` with the usual
`--text` and `--idempotency-key` options.
The JSON needs `command`, `revision`, `object_format`, `reported_by`,
`environment`, `result`, `evidence`, and `evidence_content`.

`evidence` is a list of file paths or source pointers. `evidence_content` is a list
of objects with a relative `path` and its `content_sha256`.
Use the real tested commit for `revision` and `sha1` or `sha256` for `object_format`.

memq saves the result you report. It does not run the test or check that your
current environment matches the test environment. Missing evidence stays unknown.
A saved view keeps the original report. File checks can change its applicability
to `stale` or `unknown` when you read it again.

## Check or rebuild memory

```sh
memq doctor
memq rebuild
```

memq keeps its local search data outside the repository.
Set `MEMQ_DATA_DIR` to choose a location. Otherwise it uses
`$XDG_DATA_HOME/memq`, or `$HOME/.local/share/memq` when XDG is not set.

Rebuild uses sources that still exist. If you delete the source documents,
notes, and their history, you must restore them from Git or a backup.
memq cannot recover context with no surviving source.
An unavailable capture store is reported in coverage. After all known records
are forgotten, it does not block an empty briefing or a new note.
The local store also keeps deletion rules and note retry identities in
`access.sqlite`. Keep that file in backups. If memq reports
`access_state_incomplete`, restore it before importing sources again.

To remove a record from memq, run `memq forget "ITEM_ID"`.
This leaves the original source file in place and saves a rule that stops the
record from being imported again. Review and commit `.memq/tombstones/` if that
rule should apply to your team.
