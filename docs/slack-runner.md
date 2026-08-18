# Run Fluent from a private Slack workspace

Fluent's Slack runner uses Socket Mode. It does not need a public request URL.

1. Create a user-owned Slack app at <https://api.slack.com/apps>, choose
   **From an app manifest**, and paste the manifest printed by:

   ```sh
   fluent slack manifest
   ```
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
5. Create `~/.config/fluent/slack/config.json` with the runner policy. For
   example:

   ```json
   {"workspace":"example","projects":{"app":{"label":"app","checkout":{"label":"main","path":"/absolute/checkout","identity":"checkout-v1:<digest>","branch":"main","commit":"<commit>"},"allowed_participants":["U123"]}},"channel_defaults":{"C123":"app"}}
   ```

   Print the machine-local opaque `identity` with:

   ```sh
   fluent slack checkout-identity /absolute/checkout
   ```

   Pin every checkout label, identity, branch, commit, and allowed participant.
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

   `fluent slack run` preflights the local Slack, checkout, and Codex
   boundaries, opens Socket Mode, and keeps receiving events until stopped.

If an agent sandbox blocks Keychain or LaunchAgent access, run the reported
host handoff command outside that sandbox. This is a host permission issue,
not a Slack login failure.
