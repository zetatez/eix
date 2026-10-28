//! Built-in prompt templates for the review and verification commands.
//!
//! These are ordinary prompt commands: the expanded text is submitted as a
//! user message, so the model can use its normal tools. `$ARGUMENTS` receives
//! whatever the user typed after the command.

pub(super) const CODE_REVIEW: &str = r#"Review the current changes for correctness, regressions and missing tests.

Start by running `git diff HEAD --no-color --no-ext-diff` (and `git status --short`). Read the surrounding code when the diff alone is not enough. Do not modify files.

Report findings grouped by severity (blocking, important, nit). For each finding give `path:line`, what is wrong, why it matters, and a concrete fix. If you find nothing, say so explicitly and list what you checked. Focus on: $ARGUMENTS"#;

pub(super) const SECURITY_REVIEW: &str = r#"Perform a security review of the current changes.

Start with `git diff HEAD --no-color --no-ext-diff` and `git status --short`. Trace untrusted input to sensitive sinks, and check for command injection, path traversal, unsafe deserialization, secret leakage, weak file permissions, SSRF, and authorization gaps. Also look at new dependencies and network calls. Do not modify files.

Report findings grouped by severity (critical, high, medium, low) with `path:line`, the attack path, impact, and a concrete remediation. If a suspected issue is not exploitable, say why. Focus on: $ARGUMENTS"#;

pub(super) const SIMPLIFY: &str = r#"Simplify the code I just changed without changing behaviour.

Run `git diff HEAD --no-color --no-ext-diff` first. Look for duplicated logic, unnecessary indirection, dead code, over-general abstractions, and error handling that is more complex than the failure deserves. Prefer deleting code over adding it. Keep the public API and observable behaviour unchanged.

Make the edits, then run the relevant formatter, build and tests. Report what you removed or merged and the exact commands you ran. Scope hint: $ARGUMENTS"#;

pub(super) const VERIFY: &str = r#"Verify the current change end to end.

1. Inspect the diff with `git diff HEAD --no-color --no-ext-diff`.
2. Identify the affected crates/modules and the narrowest meaningful checks.
3. Run the formatter check, build/clippy and the relevant tests. Add a focused test if the change has no coverage and the behaviour is testable.
4. If the project has a runnable app or CLI, exercise the changed path and capture the output.
5. Report each command with its result. Do not claim success without evidence; state explicitly what could not be verified and why.

Scope hint: $ARGUMENTS"#;
