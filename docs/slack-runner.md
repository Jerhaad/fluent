# Run Fluent from a private Slack workspace

Fluent's Slack runner uses Socket Mode. It does not need a public request URL.

1. Create a user-owned Slack app at <https://api.slack.com/apps>, choose
   **From an app manifest**, and paste the `SOCKET_MODE_MANIFEST` printed in
   `src/slack_collaboration.rs`.
2. Install the app into the intended workspace. In **Socket Mode**, create an
   app-level token with the `connections:write` scope. Copy its `xapp-` token.
3. In **OAuth & Permissions**, copy the bot `xoxb-` token after installation.
   Never put either token in a config file, shell profile, log, or Slack post.
4. On the runner owner's Mac, store the tokens in the login Keychain under
   service `dev.fluent.slack` and accounts `app-token` and `bot-token`. The
   runner reads those names only; it redacts token-shaped values in status.
5. Create a `RunnerConfig` that names the Slack workspace and explicitly pins
   each checkout label, identity, branch, commit, and allowed participant. Run
   preflight before accepting events; invalid or changed checkouts fail closed.
6. Install the exact Fluent executable as the owner-scoped LaunchAgent with
   `LaunchAgentServiceManager`, then use its `install`, `start`, `restart`,
   `stop`, and `enabled` controls. This starts it at login and keeps it local.

If an agent sandbox blocks Keychain or LaunchAgent access, run the reported
host handoff command outside that sandbox. This is a host permission issue,
not a Slack login failure.
