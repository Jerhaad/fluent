# Run Fluent from a private Slack workspace

Fluent's Slack runner uses Socket Mode. It does not need a public request URL.

1. Create a user-owned Slack app at <https://api.slack.com/apps>, choose
   **From an app manifest**, and paste the manifest printed by:

   ```sh
   fluent slack manifest
   ```
   If Fluent is already installed in the Slack workspace, update that app with
   this manifest and reinstall it. Alternatively, enable **Interactivity &
   Shortcuts** in the app settings before reinstalling. This enables the
   revision-bound approval controls used by project approvers.
2. Install the app into the intended workspace. In **Socket Mode**, create an
   app-level token with the `connections:write` scope. Copy its `xapp-` token.
3. In **OAuth & Permissions**, copy the bot `xoxb-` token after installation.
   Never put either token in a config file, shell profile, log, or Slack post.
4. On the runner owner's Mac, store the tokens in the login Keychain under
   service `dev.fluent.slack` and accounts `app-token` and `bot-token`. The
   runner reads those names only; it redacts token-shaped values in status.

   Enter each token without placing it in shell history:

   ```sh
   read -r -s -p "Slack app token: " FLUENT_SLACK_TOKEN; echo
   security add-generic-password -U -a app-token -s dev.fluent.slack -w "$FLUENT_SLACK_TOKEN"
   unset FLUENT_SLACK_TOKEN

   read -r -s -p "Slack bot token: " FLUENT_SLACK_TOKEN; echo
   security add-generic-password -U -a bot-token -s dev.fluent.slack -w "$FLUENT_SLACK_TOKEN"
   unset FLUENT_SLACK_TOKEN
   ```
5. Create the runner configuration directory and then create
   `~/.config/fluent/slack/config.json` with the runner policy:

   ```sh
   mkdir -p ~/.config/fluent/slack
   ```

   For example, write this policy to `config.json`:

   ```json
   {"workspace":"example","enabled_providers":["codex","claude"],"projects":{"app":{"label":"app","checkout":{"label":"main","path":"/absolute/checkout","identity":"checkout-v1:<digest>","branch":"main","commit":"<commit>"},"allowed_participants":["U123"],"default_provider":"codex","enabled_providers":["codex","claude"],"project_approvers":["U123"],"github_remote":"https://github.com/example/app"}},"channel_defaults":{"C123":"app"},"direct_message_defaults":{"D123":"app"}}
   ```

   Print the machine-local opaque `identity` with:

   ```sh
   fluent slack checkout-identity /absolute/checkout
   ```

   Pin every checkout label, identity, branch, commit, and allowed participant.
   Set the top-level `enabled_providers` to every conversation provider
   installed and authenticated for this runner. Each project's
   `enabled_providers` may narrow that local allowlist but cannot add to it. Set
   `project_approvers` to the allowed participants who may approve Brief,
   Behaviors, Approach, and Plan revisions or authorize execution, resume, and
   landing actions. Every project approver must also appear in
   `allowed_participants`; add a person's `U...` member ID to both lists when
   they may contribute and approve actions. A valid policy may omit project
   approvers, but then no one can approve a planning revision or authorize the
   other lifecycle actions.
   The identity binds the canonical checkout and Git administrative directory
   to their filesystem identities. A checkout or Git directory that is
   replaced or recreated, or whose branch or commit cannot be inspected, fails
   closed until the runner owner records a new identity.
6. Before enabling Slack, register every configured checkout with the
   scheduler. `fluent scheduler run` registers its current checkout before it
   starts or checks the owner-wide scheduler service. Run these commands
   sequentially with the same Fluent executable.

   In one terminal, start the service from the first checkout and keep that
   process running:

   ```sh
   cd /absolute/app-checkout
   fluent scheduler run
   ```

   After `fluent scheduler status` reports that service as running, use another
   terminal to register each additional configured checkout. For example:

   ```sh
   cd /absolute/api-checkout
   fluent scheduler run
   ```

   Because the first terminal already owns the service, each additional
   registration exits after reporting `scheduler service is already running`.
   That message is expected: registration completes before the singleton
   service check. Repeat the command from every remaining checkout; do not run
   the registration commands concurrently.

   Finally, check each registered project and wait until it reports
   `Scheduler: running` with the expected build:

   ```sh
   cd /absolute/app-checkout
   fluent scheduler status

   cd /absolute/api-checkout
   fluent scheduler status
   ```

   Only then enable the multi-project Slack runner. Its preflight checks the
   scheduler request boundary before it begins accepting Slack events.

7. Print this guide at any time with `fluent slack setup`. Enable the exact
   Fluent executable as an owner-scoped LaunchAgent with:

   ```sh
   fluent slack enable
   ```

   Inspect its safe status with `fluent slack status`, restart it with
   `fluent slack restart`, and stop it with `fluent slack stop`.

   `fluent slack run` preflights the local Slack, checkout, and provider
   boundaries, opens Socket Mode, and keeps receiving events until stopped.

If an agent sandbox blocks Keychain or LaunchAgent access, run the reported
host handoff command outside that sandbox. This is a host permission issue,
not a Slack login failure.

## Start the first Fluent thread

Invite the Fluent app to each configured channel before using it, for example
with `/invite @Fluent`. In Slack, open the channel details and copy the channel
ID (`C...`) for the `channel_defaults` key. Open each allowed participant's
profile, choose
**Copy member ID**, and add that `U...` value to `allowed_participants`.

An allowed participant starts a thread by posting a top-level mention in the
configured channel, for example:

```text
@Fluent plan the next release
```

Continue collaborating with ordinary replies in the resulting thread. When a
channel has no available default project, Fluent keeps the thread unbound and
offers the configured projects; it does not grant provider checkout access
until an allowed participant selects an available project.

Fluent presents approvers with a revision-bound **Approve** control when it
requests authorization for a lifecycle action. The control authorizes only the
displayed action and revision. Completed or revised planning artifacts follow
the separate thread reply flow described below. A participant may contribute
context, but only a configured project approver may use the control or approve
a planning revision.

## Review planning artifacts

To bind a planning review to an existing Fluent Work Item, include its ID in
the opening request:

```text
@Fluent plan the next release work-item: 20260817-next-release
```

Fluent posts each completed or revised Brief, Behaviors, Approach, or Plan as
a named planning revision in that thread. Review the exact Markdown in the
post, then reply in the same thread with its revision ID:

```text
approve <revision-id>
```

When an artifact spans several Slack posts, their ordered parts are one exact
revision; approve the revision ID shown on those posts only after reviewing all
parts. Fluent holds planning at that revision until an approver replies. Only
a configured `project_approvers` member may approve it, and approval advances
the bound Work Item's corresponding planning stage.

## Request another project

An active Fluent thread remains bound to the project that started it. To work
with another configured project, reply in the thread with:

```text
project: other-project
```

Fluent keeps the current thread and its history bound to the original project,
then offers a linked Fluent thread for `other-project`. Use the linked thread
for the new project's checkout and decisions.

## Preserve Slack reply history

If you edit a reply before Fluent processes it, Fluent uses the latest Slack
text as that contribution. After Fluent has processed a reply, an edit becomes
an attributed correction: Fluent retains the contribution it used and records
the new text alongside it. Deleting a processed Slack reply similarly marks its
source as deleted without removing the contribution from Fluent's history.

## Configure direct messages

To use a direct message, open its details in Slack and copy the DM conversation
ID (`D...`). Add that ID to `direct_message_defaults`, as shown in the example
configuration above. A top-level `@Fluent` request in that DM binds and routes
the thread under the same checkout, allowed-participant, and project-approver
policy as a channel. Reinstall the app after updating its manifest so Slack can
deliver `message.im` events and grant the `im:history` scope.

## Ask about code

Participants can ask code questions in a bound Fluent thread. Fluent keeps
source locations repository-relative and includes line numbers. When the code
is committed and the checkout has a supported GitHub remote, Fluent posts a
permalink pinned to the exact commit. For uncommitted code or repositories
without a supported remote, Fluent labels the reference as local-only and
posts a bounded excerpt or diff instead of claiming a durable link. Replies
never expose the runner's absolute checkout path.

## Provider policy and recovery

`default_provider` selects the provider for a new thread; omit a selection to
use it. The top-level `enabled_providers` is the runner's authoritative local
allowlist, and each project's allowlist must be its subset. The runner supports
`codex` and `claude`; enable only adapters whose local executable is installed
and authenticated for the runner owner. Override their executable paths with
`FLUENT_CODEX_APP_SERVER` and `FLUENT_CLAUDE`. Both adapters run with the exact
bound checkout and private provider state beneath the Slice draft. Set the
fair cross-thread provider capacity with `FLUENT_SLACK_TURN_CAPACITY` (default
four).

Authenticate each thread's selected executable through Fluent so the login
uses the same private homes as provider turns. Take the thread timestamp from
the Slack thread URL, then run:

```sh
fluent slack provider-login --project app --thread 1712345678.123456 --provider codex
fluent slack provider-login --project app --thread 1712345678.123456 --provider claude
```

The Codex command runs `codex login` in the thread's private `CODEX_HOME`. The
Claude command opens the interactive Claude client with the thread's private
`CLAUDE_CONFIG_DIR`; enter `/login` there and finish its prompts. Fluent honors
`FLUENT_CODEX_APP_SERVER` and `FLUENT_CLAUDE`, so this command authenticates
the exact configured executable. Successful authentication queues the
provider-login recovery action for the running owner process. That process
rechecks the selected provider and exact thread scope, then resumes the
preserved turn. Run this command as the same local account that owns the
runner. Restarting or reconnecting Slack alone leaves the turn paused.

To select a provider explicitly, include its allowed name in the opening
mention. For example:

```text
@Fluent [provider=codex] plan the next release
```

The requested name must appear in the project's `enabled_providers` list. The
runner validates it before binding a thread. When a turn is interrupted, a
configured project approver can post `@Fluent [provider=claude]` (or `codex`)
in that thread. Each `project_approvers` entry is a Slack member ID authorized
to perform that switch; an empty set removes the provider-switch recovery
action. Fluent switches adapters, queues the preserved turn, and resumes it
when fair provider capacity is available without changing scheduler-owned
Work.
Fluent reports authentication requirements, denied runner-host credential
access, unavailable runtimes, incompatible provider protocols, and rate
limits as distinct recovery states. Failed turns stay durable and paused, and
the runner does not repeatedly invoke the provider or post recovery messages.
After a scoped provider login succeeds, the runner rechecks the selected
provider in the bound checkout before it re-admits the preserved turn. The
status offers provider switching when the project has a configured approver.
Restore host credentials for access denials; update Fluent or the provider
executable when their structured-session protocols are incompatible.
Rate-limited turns retain the provider's retry deadline and resume
automatically; replies received meanwhile are acknowledged, retained in Slack
order, and sent together as the next attributed turn. If the exact bound
checkout disappears or its identity changes, Fluent stops provider access,
reports the unavailable binding, and never substitutes another checkout
automatically. After restoring the exact checkout, invoke the runner's
checkout-reconfirmation route as the runner owner:

```sh
fluent slack reconfirm-checkout --project app --thread 1712345678.123456
```

The command verifies that the configured identity, branch, and commit match
the restored checkout before it queues the action. The running owner process
validates the thread's original project binding again before it re-admits the
preserved turn. Restarting or reconnecting Slack alone leaves the turn paused.
Fluent retains the original project binding throughout recovery.

## Control and report Work from the thread

Fluent keeps planning and execution as separate approvals. A configured
`project_approvers` member can approve a plan to materialize its proposed Work
Items without starting them. A later execution approval submits the same
versioned request identity for each Work Item to the local scheduler, so a
retry cannot create another execution request.

The conversation provider presents the exact plan-approval action in the
thread. Its durable command has this shape:

```text
approve plan {"slice_id":"slice-release","planned_work":[{"id":"release-ui","title":"Implement the release UI"}]}
```

An approver submits `authorize execution` in the same thread only after the
plan action succeeds. The runner validates the Slack actor against the bound
project for both actions; other conversation text continues to the provider.

The versioned scheduler service validates the registered checkout and running
build before it admits that request to Fluent's real durable queue. It owns
capacity-limited claims and Attempt workers across registered projects, and
reports status from the bound queue dispatch rather than from an unconsumed
submission token.

The runner posts scheduler outcomes and authoritative Work lifecycle changes
back to the originating thread. Transition replies identify the Work object,
origin, reason, elapsed time, and bounded evidence reference. Ready-candidate
replies include the verified commit, completed tests and reviews, and review
links when the configured remote permits them. Failed or `needs-user` replies
state the permitted recovery action; Slack never mutates GitHub state.

Status comes from persisted Work rather than chat text. The thread projection
shows the project and checkout, Slice stage, provider state, each Work state,
and the next action. An allowed participant requests that projection by
replying `status` or `work status` in the Fluent thread. After a runner restart,
Fluent replays only transitions whose stable delivery identities have not
already been scheduled. A Slice becomes complete only when every Work Item's
latest required Attempt is `complete`, every candidate has landed successfully,
and all required post-land processing has passed. A failed or `needs-user`
latest Attempt, an unlanded candidate, or a post-land failure keeps the Slice
active.
