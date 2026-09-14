# GitHub checks and releases

Push code or open a pull request to run the checks.
Set up GitHub rules and Codex reviews separately.

## Checks

Pull requests to `main` and pushes to `main` check formatting, code quality,
Linux and macOS tests, license notices, and the oldest supported Rust version.
They use the exact dependency versions in `Cargo.lock`.
Contributors do not need access to repository secrets.
Linux jobs select GCC 12 to build the native vector library.

`CI required` passes only when all required checks pass.
After the first successful GitHub run, require this check in the `main` branch rules.
Allow the Actions used by the workflows. Enable private security reports in
the repository settings.

## Codex reviews

1. Connect the repository through [Codex cloud](https://developers.openai.com/codex/cloud).
2. Enable **Code review** for the repository and **Automatic reviews** in Codex settings.
3. Open a pull request to check that the review runs.

You can also comment `@codex review` on a pull request.
[AGENTS.md](../AGENTS.md) gives Codex the project review rules.
A maintainer still decides whether to merge.

The workflow files do not install the Codex app or turn on reviews.
See the [official setup guide](https://developers.openai.com/codex/third-party/github).

## Releases

1. Limit who can create or change release tags in the repository rules.
2. Tag a commit on `main` as `vMAJOR.MINOR.PATCH`. The version must match `Cargo.toml`.
3. Check the draft release, then publish it when ready.

A release runs all checks on the tagged commit. It builds and checks the Linux
x86-64 and macOS Apple Silicon programs, then creates a **draft** GitHub release.
Each download includes memq, the MIT license, and dependency license notices.
Only the final job has permission to write a release.

Before publishing, read the release notes and check both downloads.
Compare them with `SHA256SUMS` to confirm the files are unchanged.

Linux builds use glibc. The workflows target Ubuntu 22.04 and macOS 14.
Other systems do not have CI coverage.
Publishing to crates.io or other package managers is not set up.
