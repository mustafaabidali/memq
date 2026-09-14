# Contributing

Start with the [usage guide](docs/usage.md) and [review rules](AGENTS.md).

memq helps agents remember work between sessions.
It works alongside fff for file search and codebase-memory-mcp for code links.

## Development

1. Install Git, Python 3.9 or newer, and Rust through `rustup`.
2. Clone the repository and open its folder.
3. Run these checks. `rust-toolchain.toml` selects the Rust version.

On Ubuntu 22.04, first [select GCC 12](docs/usage.md#build-tools-on-ubuntu-2204).

```sh
cargo build --locked
./scripts/check.sh
python3 -m unittest discover -s tests/packaging -p 'test_*.py'
```

Tests use temporary Git repositories and made-up records.
Test what users can observe. For a bug fix, show that the test fails before the
fix and passes after it.

## Code map

Start with the part that owns the behavior you want to change:

| Part | Where to start | Responsibility |
| --- | --- | --- |
| Commands and core | [core.rs](src/core.rs), [operations.rs](src/operations.rs) | The CLI, MCP, and hooks call the same core. The core controls refresh, reads, writes, and repair. |
| Durable memory | [notes.rs](src/notes.rs), [store.rs](src/store.rs) | Notes and deletion rules live in files. Storage owns index updates, recovery, and complete published views. |
| Source evidence | [capture.rs](src/capture.rs), [policy.rs](src/policy.rs) | Source adapters read records. Shared rules check identity, scope, approval, and redaction. |
| Search | [retrieval.rs](src/retrieval.rs), [vectors.rs](src/vectors.rs) | Text and vector search find evidence. They receive the data they need and do not call back into the core. |
| Replies | [presentation.rs](src/presentation.rs), [budget.rs](src/budget.rs) | Formatting preserves evidence and warnings. Budgeting counts the actual reply sent to the agent. |

Keep the layers clear. Commands must not run SQL or repair the database.
Formatting must not read files, query storage, or start processes.
Each harness format has its own parser; identity checks and redaction are shared.
Keep schema changes inside storage and preserve its transaction boundaries.

The local index is replaceable. Notes, source documents, and deletion rules are
the durable inputs. Rebuild from surviving inputs; report missing evidence.
The separate access ledger preserves deletions and note retry identities.
Keep it when replacing the index.
If all copies are deleted, the user must restore them from Git or a backup.

Split code when responsibilities differ. Avoid wrappers that only forward calls.
Measure a performance change against the same input and check the same answers.

## Changes and reviews

Open an issue before making a large change to commands or stored data.
In your pull request, explain the problem, the fix, and how you checked it.
For a speed claim, include the data size, machine, tool versions, and whether
the cache was empty or already filled.
See [GitHub checks and releases](docs/releasing.md) for release and Codex review setup.

Keep saved records and other people's staged changes safe.
A proposed decision must not replace an approved one.
A search result or an agent's claim does not prove that a feature works.
Show when data is missing or out of date.

Use made-up examples in issues and tests. Remove passwords, private chats,
personal file paths, and real session IDs.
Contributions use the [MIT license](LICENSE).
