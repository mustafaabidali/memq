# memq

Help your coding agent remember past work.

memq saves decisions and progress, then gives each new session a short briefing.
It works inside Git repositories with Codex, OpenCode, and OMP.

On Ubuntu 22.04, [set up the compiler first](docs/usage.md#build-tools-on-ubuntu-2204).

```sh
cargo install --path . --locked
cd /path/to/your/repository
memq init
memq brief --compact
```

[Usage](docs/usage.md) · [Contributing](CONTRIBUTING.md)

Use [fff](https://github.com/dmtrKovalenko/fff) for fast file search and
[codebase-memory-mcp](https://github.com/DeusData/codebase-memory-mcp) to find links between code.

[MIT](LICENSE).
