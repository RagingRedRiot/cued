# Recipes

Example TOML workflows for cued. Read and adapt a recipe before submitting it;
cued does not discover or run these files automatically.

- [`daily-checks.toml`](daily-checks.toml): build a project, run its tests only
  if the build succeeds, and notify on failure or a held run. Requires GNU Make
  and a project with `build` and `test` targets. Submit it from that project's
  directory; the schedule is daily at 09:00 in your local time zone. The targets
  determine any files created, changed, or deleted.

Commands run with your account's privileges and are not sandboxed. Recipes are
reviewed through normal pull requests. Review checks an example's behavior and
documentation; you still need to review its commands for your own machine.

## Example verification

`daily-checks.toml` was verified with cued 0.1.2 and GNU Make 4.3 using fixture
targets that succeed or deliberately fail. Both successful steps completed;
a failed build skipped the test step, and a failed test ended the run as failed.
Both failure cases queued the expected notification. The daily schedule was
accepted unchanged; execution checks used a CLI interval override to run promptly.
Desktop popup delivery and interruption recovery were not exercised in this check.

## Contributing a recipe

Write your recipe as a TOML workflow file outside cued, then submit it to cued
and verify its behavior before opening a pull request. Describe how you tested
its success and failure paths, along with any prerequisites.

Use the [recipe PR template](https://github.com/RagingRedRiot/cued/compare?expand=1&template=recipe.md)
when opening your pull request. Select your contribution branch on the comparison
page. For an existing comparison URL, add `template=recipe.md` to its query parameters.

It is not recommended to use `cued show ID --toml` to prepare a contributed recipe.
Job exports describe an existing job and can include machine-specific settings.
A contributed recipe should be an intentionally authored, reusable example.

Prefer defining commands directly in the TOML where practical, so their logic
is reviewable with the routing. If a recipe invokes external scripts, document
them and ensure they and their dependencies cannot be modified or replaced by
untrusted users. Job approval does not freeze referenced files. See the
[security policy](../SECURITY.md#scope-and-security-boundaries) for the trust model
and permission guidance.

Each contribution should explain:

- What it does, including its success and failure paths.
- Required tools, versions, and local configuration.
- Files it creates, changes, or deletes.
- How its commands and routing were verified.

Do not include secrets or personal paths in contributed recipes. Contributed
recipes are licensed under the repository's [MIT license](../LICENSE).
