# Security policy

## Reporting a vulnerability

Report suspected vulnerabilities through
[GitHub's private vulnerability reporting](https://github.com/RagingRedRiot/cued/security/advisories/new).
Private reporting is enabled for this repository. Do not post exploit details
in a public issue or pull request before maintainers have assessed the report.

Please include:

- The affected version or commit, installation method, and Linux environment.
- A minimal reproduction and any relevant configuration.
- The observed behavior, expected security boundary, and potential impact.
- Any prerequisites, such as local account access or MCP policy settings.
- What you verified and what remains uncertain.

Use synthetic credentials and data where possible. Do not include real secrets
or information belonging to other users. AI-assisted reports are welcome;
verify generated claims and distinguish observed behavior from speculation.
See the [contribution policy](CONTRIBUTING.md#ai-assisted-contributions).

## Scope and security boundaries

cued is intentionally designed as a single-user Linux scheduler. Scheduling
and execution operate within that user's existing permissions; cued does not
provide a separate execution identity or isolation boundary within the account.
The model assumes that the account authorized to control cued and read its
database and output is already trusted to run the same commands in a terminal
and access the environment values available to that account.

Its security model protects a user's jobs and local IPC from other local users
and enforces configured MCP capabilities and human approval requirements.
Those MCP restrictions are intended for users who give an AI access through MCP
without granting independent access to cued's CLI or local files. Requests through
MCP remain subject to the configured checks, but those checks do not constrain
an agent's separate shell or filesystem access under the same account. Granting
that access gives the agent other ways to act with the user's privileges; MCP
policy is not an operating-system sandbox.

Reports about bypassing cued's cross-user protections, configured MCP checks,
or approval requirements, or unintentionally exposing protected data, are welcome.

Workflow commands run with the daemon user's privileges. Recipes and job
approval do not sandbox commands; intentionally submitted commands and
same-user processes are trusted with those privileges. A command can also
exercise separately configured elevation available to that user. Read and
review recipes before submitting them.

Prefer defining commands directly in the workflow TOML where practical, so their
logic can be reviewed alongside the schedule and routing. Approval binds the
stored job definition; it does not freeze referenced scripts, executables, or
other files. Changes to those files can change what a later run executes.

When invoking a script, ensure it can be modified only by the scheduler's user
or trusted administrators. Protect its dependencies and any directories that
allow the script to be replaced as well. Ownership alone is insufficient if
group permissions, ACLs, or directory permissions let an untrusted user alter
or replace the code. Such a user could cause the next scheduled run to execute
their code with the scheduler user's privileges. Defining commands in TOML does
not remove the need to trust the executables and files those commands use.

Captured environment values and command output can contain sensitive data.
The store is not encrypted, and secret filtering and redaction cannot identify
every secret. Keep this in mind when sharing logs or inspecting jobs with
`cued show --json`.

These documented limitations do not rule out reports of defects in the
protections cued claims to provide. See [DESIGN.md, section 7](DESIGN.md#7-security-and-approval)
for the threat model and approval contracts.

## Running as root

**Setting up or running cued as root is strongly discouraged.** Install and run
the scheduler for an unprivileged user account instead; it does not require a
root daemon.

A daemon running as root executes workflow commands with root's privileges.
A mistaken command, unsafe recipe, or overly permissive MCP configuration can
therefore affect the entire system. cued's graph validation and approval controls
do not make root execution safe or sandbox its effects.

This recommendation concerns the account running the scheduler, not administrator
ownership of a binary installed in a shared location.

## Expected behavior and report eligibility

Reports that demonstrate only the documented behavior of an authorized job or
an explicitly permitted configuration are not considered vulnerabilities in cued.
Examples include:

- An intentionally submitted command reading or modifying files its account
  can already access, or producing destructive effects within those permissions.
- A trusted same-user process reading that account's job database or output.
- An MCP client executing commands when the user deliberately configured
  execution access as `open`, without bypassing another applicable restriction.
- Commands executing as root because the scheduler was deliberately run as root.

These choices can be dangerous, but the risk alone does not demonstrate a breach
of cued's security model. A report should identify a protection cued claims to
provide and show how that protection is violated.

Configuration-dependent vulnerabilities are still valid when they bypass an
intended protection. Cross-user access, approval or capability bypasses, and
unintended disclosure through a restricted interface remain in scope. If you
are unsure whether a finding crosses a boundary, report it privately with the
reproduction and relevant configuration.

## Supported versions

cued is early alpha. Security fixes target `main` and the latest release line;
older releases do not have guaranteed backports. Reports affecting older versions
are still welcome, especially if the issue may also affect current code.

Security updates may require changes to commands, storage, or protocols. Alpha
releases do not promise compatibility or migration support.

## Response and disclosure

Maintainer capacity is limited; there is no guaranteed response or resolution
time. Maintainers will use the private report to discuss reproduction, impact,
and a fix or mitigation where appropriate.

Please coordinate public disclosure through the private report so affected users
can receive a fix or mitigation before exploit details are published.
