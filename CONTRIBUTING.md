# Contributing to cued

We welcome reproducible bug reports, focused fixes, verified recipes, tests,
and documentation improvements. AI-assisted contributions are welcome and
encouraged. The same standards apply regardless of the tools used.

## Before starting

Small fixes and useful recipes can go straight to a pull request. For substantial
features, new dependencies, architectural changes, or broad refactors, open an
issue to discuss the need and approach before implementing them. Check existing
issues and pull requests to avoid duplicating work.

If you are new to the project, start with one focused pull request and wait for
feedback before opening several more. You do not need to open an issue for every
small change.

## Reporting bugs

Include your cued version, Linux distribution, installation method, expected
behavior, actual behavior, and a minimal reproduction. For desktop problems,
include your desktop environment and whether you use Wayland or X11.

Share relevant logs or a minimal workflow when useful, but remove secrets and
personal information first. Inspect command output and configuration before
posting them; `cued show --json` can expose captured environment values.

For suspected vulnerabilities, follow [SECURITY.md](SECURITY.md) and use the
private reporting route rather than a public issue or pull request.

## Development and checks

Development targets Linux. The CLI and daemon require Rust 1.89 or newer; the
optional desktop status window requires Rust 1.95 or newer. Install the `rustfmt`
and `clippy` components for formatting and lint checks.

For the headless package:

```sh
cargo build -p cued --locked
cargo test -p cued --locked
cargo clippy -p cued --all-targets --locked -- -D warnings
```

For the whole workspace, including the status window:

```sh
cargo build --workspace --locked
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
```

Check formatting with `cargo fmt --all --check`. Run the relevant tests first,
then broader checks as appropriate for the change. Report the checks you actually
ran and any limitations; recipes and documentation changes do not require you
to exercise unrelated GUI behavior.

Some notification tests require `/usr/bin/dbus-daemon`; their private test buses
do not require a running desktop session. Checking actual desktop popup delivery
or the status window requires a desktop session. See the
[README](README.md#install-and-upgrade) for the window's runtime requirements.

The cross-user access fixture requires Docker and access to its engine:

```sh
cargo build -p cued --locked
scripts/security/run.sh target/debug/cued
```

It runs against isolated users inside a container. The optional `test-hooks`
feature is for fault-injection tests; do not enable it in installations used for
real jobs. See [DESIGN.md](DESIGN.md) for the execution contracts and test scope.

## Submitting patches

Use the default code-change PR template. Explain the concrete problem, the
resulting behavior, and relevant validation results. Keep each pull request
focused on one coherent change and avoid unrelated formatting or refactoring.

Add or update relevant tests for behavior changes and documentation for changes
users need to understand. Call out compatibility or migration implications.
Scheduling, recovery, approval, storage, and protocol changes need particular
care: explain how the change relates to the contracts in [DESIGN.md](DESIGN.md).

Follow the surrounding code style. Pull requests do not change the release
version; maintainers handle versioning through the release workflow.

CI runs formatting, lint, workspace tests, and the cross-user access fixture.
The separate controls check requires a maintainer to apply `reviewed-controls`
to the current revision when a PR changes files under `.github/`, the access
fixture under `scripts/security/`, or existing files under `tests/`. Adding a
new test file under `tests/` does not by itself require that label. A later push
requires renewed maintainer review. An expected controls failure is not a reason
to weaken or bypass the check.

## Submitting recipes

Use the [recipe contribution guidance](recipes/README.md#contributing-a-recipe)
and its dedicated PR template. Write the TOML outside cued and verify its behavior
in cued; we do not recommend using job exports to prepare contributed recipes.

Document prerequisites, commands and side effects, and how you exercised the
success and failure paths. State what remains untested. Recipes are examples
reviewed through normal pull requests, and their commands run with the user's
privileges without sandboxing.

## AI-assisted contributions

Before submitting, review the entire change, understand what it does, and verify
its behavior. You should be able to explain the change, answer review questions,
and address problems found during review. AI-generated claims about tests or
behavior are not verification.

Briefly mention substantial AI assistance in the PR description, along with how
you checked the result. Prompt transcripts and disclosure of routine autocomplete
are unnecessary. Keep explanations concise and accurate.

Automated submissions require human review before posting. Do not submit batches
of speculative fixes, generated recipes, or review comments. These requirements
apply to issues and review discussion as well as pull requests.

## Review expectations and conduct

Maintainer review time is limited, and there is no promised turnaround.
Contributions may be deferred or closed because of scope, insufficient
verification, duplication, or maintenance cost, even when technically correct.
Maintainers may close unsuitable contributions without a detailed review or
repairing them on the contributor's behalf.

Respond to feedback and explain revisions. A closed PR does not prevent a revised
proposal that addresses the reason for closure. Repeated submissions that
disregard feedback may result in contribution restrictions.

Keep discussion respectful and constructive. Critique the work, avoid personal
attacks, and respect maintainers' decisions about project scope and review capacity.

## Licensing

Contributions, including recipes and AI-assisted work, use the project's
[MIT license](LICENSE). Submit only material you are entitled to contribute under
those terms. Do not include secrets, personal paths, or third-party material with
incompatible licensing.
