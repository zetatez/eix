# Approvals, sandboxing, and permission rules

Every tool call passes through one permission decision first. There are exactly four outcomes:
allow, ask the user, deny, or run inside the sandbox. The order is fixed: explicit rules first,
then the tool's own risk level.

## 1. Anatomy of a rule

Rules live in `[[tools.rules]]` (annotated examples are in `config.tmpl.toml`):

```toml
[[tools.rules]]
id = "deny-rm"
tool = "exec_command"
command_regex = "^rm "
action = "deny"
```

| Field | Meaning |
| --- | --- |
| `id` | Stable identifier, surfaced in approval payloads and audit logs |
| `tool` | Tool name such as `apply_patch`, `exec_command`, `web_fetch`, `mcp__*`, or `"*"` |
| `path` | Workspace-relative glob matched against the call's `path` argument; for `web_fetch` it matches the URL |
| `command` | argv prefix, for example `["git", "status"]` |
| `command_regex` | Regular expression, an escape hatch; prefer `command` when it suffices |
| `signature` | Exact JSON arguments, for tools that have neither path nor command |
| `action` | `allow` / `ask` / `deny` / `sandbox`; defaults to `ask` |

Every field you write in a rule must match for the rule to apply. All fields except `tool` are
optional, and the more you specify, the narrower the rule.

## 2. Precedence

All matching rules take part in the decision; it is not "first match wins":

1. If any `deny` rule matches, the call is denied. Deny always wins.
2. Otherwise the **most specific** matching `allow` / `ask` / `sandbox` rule applies. Specificity
   is the number of constraint fields present among `tool != "*"`, `path`, `command`,
   `command_regex`, and `signature`, one point each.

This makes "broad allow plus narrow tightening" safe: allow a wide range, then override the
exceptions with more specific `deny` or `ask` rules.

## 3. Two details of command matching

`command` is an argv token prefix, not a string prefix, so `["git", "status"]` does not also match
`git statusfoo`.

**Compound commands.** `cargo test && rm -rf x` is split into shell segments. For
`allow`/`ask`/`sandbox` rules, **every** segment must start with the prefix for the rule to match.
That blocks the "legal prefix, dangerous tail" bypass.

**Dynamic commands.** Anything that cannot be resolved statically, such as variable expansion or
command substitution, is never allowed by a command-prefix rule. `deny` rules still try to match
such commands. Constructs like `eval` fall through to `ask` by default.

`command_regex` is compiled and validated when the config loads. A bad pattern fails loudly with
`tools.rules[N].command_regex is not a valid regex` instead of silently doing nothing.

## 4. The `sandbox` action

`action = "sandbox"` means "allow only when a sandbox is available, otherwise fall back to
asking". It fits commands that should run, but that you want isolated from the filesystem:

```toml
[[tools.rules]]
id = "cargo-test-sandbox"
tool = "exec_command"
command = ["cargo", "test"]
action = "sandbox"
```

## 5. The sandbox itself

The sandbox applies to `exec_command` only and is configured under `[tools]`:

| Field | Default | Meaning |
| --- | --- | --- |
| `sandbox` | `"auto"` | `auto` uses bubblewrap when present and runs directly otherwise; `required` fails without a sandbox; `off` disables it |
| `sandbox_network` | `false` | Whether the sandbox may reach the network |
| `sandbox_read_paths` | empty | Extra read-only mounts, `~` supported; the whole home directory is not mounted by default |
| `sandbox_writable_roots` | empty | Extra writable paths, resolved relative to the workspace; the workspace itself is always writable |

`auto` is best effort and not an isolation guarantee: with no bubblewrap installed, commands run
directly on the host. Use `required` when you need a hard guarantee, since it prefers failing over
degrading.

Toolchain caches (`~/.cargo`, `~/.rustup`, and so on) are invisible by default. Add them to
`sandbox_read_paths` when a build needs them.

## 6. Snapshots and diffs

Two further layers of protection sit outside the permission system, both under `[tools]`:

| Field | Default | Meaning |
| --- | --- | --- |
| `snapshots` | on | Save a rollback-able snapshot before any mutating tool runs |
| `auto_apply_workspace_edits` | on | Apply low/medium-risk writes directly instead of approving each one; snapshots still apply |
| `turn_diff` | on | Emit a `turn.diff` notification at the end of each turn with the diff of tracked files |

Setting `auto_apply_workspace_edits = false` restores "review a diff before every file change".

## 7. Common recipes

```toml
# Allow read-only checks wholesale
[[tools.rules]]
id = "read-only-shell"
tool = "exec_command"
command = ["cargo", "check"]
action = "allow"

# Never run destructive commands
[[tools.rules]]
id = "deny-rm"
tool = "exec_command"
command_regex = "^rm "
action = "deny"

# Only ask about edits under one directory
[[tools.rules]]
id = "review-src"
tool = "apply_patch"
path = "src/**"
action = "ask"

# Allow one documentation site
[[tools.rules]]
id = "docs-web"
tool = "web_fetch"
path = "https://docs.rs/**"
action = "allow"
```

The last rule shows the dual meaning of `path`: filesystem paths for ordinary tools, URLs for
`web_fetch`.

## 8. Troubleshooting

**A rule has no effect.** Check that `tool` matches the name the model actually sees: MCP tools are
`mcp__server__tool`, not their bare name. `path` is workspace-relative, so absolute paths never
match.

**An allow rule still prompts.** A more specific rule probably matched with `ask`, or a `deny`
matched. Deny is unconditional; specificity only ranks `allow` / `ask` / `sandbox`.

**The sandbox seems inactive.** With `auto` and no bubblewrap, commands simply run on the host.
Verify with `required`, or run `id` inside the sandbox and check whether the output comes from an
isolated environment.

**A command is rejected for no visible reason.** Compound commands require every segment to match
the prefix, so `cargo test && rm -rf x` does not match an allow rule for
`command = ["cargo", "test"]`. That is intentional.
