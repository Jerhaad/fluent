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
   {"workspace":"example","enabled_providers":["codex"],"projects":{"app":{"label":"app","checkout":{"label":"main","path":"/absolute/checkout","identity":"checkout-v1:<digest>","branch":"main","commit":"<commit>"},"allowed_participants":["U123"],"default_provider":"codex","enabled_providers":["codex"],"project_approvers":["U123"]}},"channel_defaults":{"C123":"app"},"direct_message_defaults":{"D123":"app"}}
   ```

   Print the machine-local opaque `identity` with:

   ```sh
   fluent slack checkout-identity /absolute/checkout
   ```

   Pin every checkout label, identity, branch, commit, and allowed participant.
   Set `enabled_providers` to the locally enabled conversation providers. Set
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
6. Print this guide at any time with `fluent slack setup`. Enable the exact
   Fluent executable as an owner-scoped LaunchAgent with:

   ```sh
   fluent slack enable
   ```

   Inspect its safe status with `fluent slack status`, restart it with
   `fluent slack restart`, and stop it with `fluent slack stop`.

   `fluent slack run` preflights the local Slack, checkout, and Codex provider
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

## Provider policy and recovery

`default_provider` selects the provider for a new thread; omit a selection to
use it. `enabled_providers` is an allowlist. The currently supported runner
adapter is `codex`; do not list `claude` until a Claude adapter is installed
for this runner. `project_approvers` reserves the approver role for a future
interrupted-thread provider-switch action; it does not grant a Slack command
in the current runner.

To select a provider explicitly, include its allowed name in the opening
mention. For example:

```text
@Fluent [provider=codex] plan the next release
```

The requested name must appear in the project's `enabled_providers` list. The
runner validates it before binding a thread. The current runner does not expose
a Slack provider-switch action or automatic provider retry. If a provider
cannot continue, preserve the thread and ask the runner owner to restore the
provider before continuing. If the exact bound checkout disappears or its
identity changes, Fluent stops provider access, reports the unavailable binding,
and never substitutes another checkout automatically.
