# Fluent – a self improving software factory

Fluent is a factory that autonomously turns your team's vision, ideas, bug reports, user feedback, production logs, and agent traces into working software.

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset=".github/assets/fluent-at-a-glance-dark.gif">
    <source media="(prefers-color-scheme: light)" srcset=".github/assets/fluent-at-a-glance-light.gif">
    <img alt="The Fluent skill lets you drive the factory by chatting with Codex, Claude Code, or another coding agent. Agent traces, production logs, user feedback, and team vision and ideas become Observations. Conversation shapes Work Items, which the scheduler delegates to a Writer, Tester, and specialist Reviewers. Reviewed changes pass through the Learner; new Observations return for later work, while Expertise informs future agents. A person or policy can approve and land the Merge Candidate before deployment. Delegated work can run in local sandboxes or on remote machines, and Fluent pulls in human context when needed." src=".github/assets/fluent-at-a-glance-light.gif" width="100%">
  </picture>
</p>

You use the factory through the Fluent [skill](https://agentskills.io/) in Codex, Claude Code, Pi, or another coding agent that supports skills. The skill turns your conversation into Fluent's interface and drives its command-line machinery for you.

### Install

```bash
npx skills add mrinalwadhwa/fluent --skill fluent  # currently macOS only
```

Use the command above to install the Fluent skill, then start your coding agent in the project folder you want Fluent to work on.

Ask it to use Fluent and describe what you want to explore, build, fix, or improve. The first invocation sets up Fluent for that project and starts shaping the work with you. You can also invoke the skill explicitly: `$fluent` in Codex, `/fluent` in Claude Code, or `/skill:fluent` in Pi.

## How Fluent works

You can capture any of those inputs as an Observation, then shape it into one or more Work Items through conversation. Approved Work can run immediately or enter a queue; when you start the scheduler, it runs queued Work as execution capacity becomes available. Each Attempt loops through Writer → Tester → Reviewers until it passes; the Learner then captures any reusable project knowledge, and Fluent returns a ready Merge Candidate for you to inspect and land. After you approve it, the land gate updates, checks, reviews, and merges the candidate into your target branch; your delivery system can deploy it from there. Findings from shaping, building, landing, and running the software can feed the next cycle as new Observations or project Expertise.

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset=".github/assets/fluent-overall-flow-dark.png">
    <source media="(prefers-color-scheme: light)" srcset=".github/assets/fluent-overall-flow-light.png">
    <img alt="Fluent separates human attention from execution capacity. In a conversation with an agent carrying the Fluent skill, you iteratively shape a Work Item. An Attempt runs Writer, Tester, Reviewers, and Learner; reviewer findings return to the Writer, and questions pause for human help without occupying an execution slot. You inspect and approve the ready Merge Candidate before a separate land gate updates, checks, reviews, and lands it. After land, Observations shape future Work and Expertise guides future Writers and Reviewers." src=".github/assets/fluent-overall-flow-light.png" width="100%">
  </picture>
</p>

People supply intent, judgment, domain knowledge, and authority to land changes. Execution environments supply model access, compute, tools, and hardware. Fluent keeps work that can run separate from work that needs a person: execution-ready Work Items wait for execution capacity, while questions and ready candidates wait for human attention. A scheduled Attempt that pauses for a person releases its execution slot, so other Work can continue.

## How you tell Fluent what to build

In the same agent conversation, start with a feature, a bug, or an Observation you recorded earlier. You do not need to arrive with a complete specification. The Fluent skill reads the relevant parts of the project, asks about decisions it cannot safely infer, and works through four questions with you:

| Artifact | What it settles |
|---|---|
| Brief | What outcome do you want, why, and what context matters? |
| Behaviors | What must be observably true when the work is done? |
| Approach | What technical direction will deliver those behaviors, and what does that choice give up? |
| Plan | What verifiable slices should the Writer reach, and which pieces can be built independently? |

You confirm each answer before moving on. Unknowns stay explicit; if a later stage exposes a missing behavior or design decision, the conversation returns to that stage instead of guessing.

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset=".github/assets/how-you-tell-fluent-dark.png">
    <source media="(prefers-color-scheme: light)" srcset=".github/assets/how-you-tell-fluent-light.png">
    <img alt="A request or selected Observation becomes a Brief, Behaviors, an Approach, and a Plan in a conversation where you confirm each stage, then becomes an approved Work Item that is not running yet" src=".github/assets/how-you-tell-fluent-light.png" width="100%">
  </picture>
</p>

After you confirm the Plan, Fluent creates one or more Work Items containing the approved context. A Work Item is the durable handoff from planning to execution: creating it does not start a coder or schedule work. Independent pieces can become peer Work Items; implementation stays sequential inside one.

For example, “add machine-readable JSON output to our CLI's status command” can become a Brief explaining why CI needs it, a Behavior saying exactly what `status --json` emits, an Approach that reuses the existing status model, and a Plan that proves one end-to-end slice before covering compatibility and documentation.

## How Fluent builds it

When you ask Fluent to run a Work Item, it starts an Attempt in an isolated worktree. In the Local Preview, the Attempt runs locally in the foreground, where you can watch each round.

Fluent creates its worktrees next to your repo, so your working tree stays clean while it builds. Place your repo at `<project>/main/` to keep all of Fluent's worktrees grouped as siblings under the project directory. Initialization prints a reminder if the directory is not named `main`.

The Writer implements the approved Plan and commits a candidate. The Tester runs the project's configured test commands. Five reviewers then inspect the same commit through separate tasks for behaviors, architecture, tests, documentation, and skills. Review tasks run in parallel up to the configured concurrency limit.

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset=".github/assets/how-fluent-builds-dark.png">
    <source media="(prefers-color-scheme: light)" srcset=".github/assets/how-fluent-builds-light.png">
    <img alt="A Work Item runs as a local foreground Attempt: the Writer produces a commit, the Tester runs project tests, five reviewers inspect it, and the Learner records project knowledge. Blocking failures return to the Writer and uncertainty pauses for you. A ready Merge Candidate still requires your acceptance before the land gate updates, checks, and reviews it for the target branch." src=".github/assets/how-fluent-builds-light.png" width="100%">
  </picture>
</p>

A new test failure or failing review verdict returns to the Writer, then Fluent tests and reviews the revision. An uncertain verdict or a decision outside the approved Behaviors and Approach pauses the Attempt at `needs-user` with the evidence collected so far instead of choosing for you. The current Local Preview can resume some infrastructure pauses in place; uncertain and exhausted-round pauses still require manual recovery.

Once the reviews pass, the Learner records reusable project knowledge and possible follow-ups. Only a successful Learner makes the Merge Candidate ready.

Ready does not mean merged. You inspect and accept the candidate first. The land gate then updates it against the current target branch, runs the configured checks and five review lenses again, and fast-forwards that branch only if they pass. If it cannot clear a conflict or finding within its bounds, it stops without moving the branch.

For the JSON status example, the Writer adds the response type and tests, the Tester runs the suite, and each reviewer checks one concern without editing the candidate it is reviewing.

## How Fluent keeps improving your code

Some findings improve the candidate being built. Others become the next piece of Work. During an Attempt, a new test failure or failing review verdict stays with the current Work Item: the Writer addresses it, then Fluent reruns the Tester and the affected reviewers.

The Learner can also record a separate change to make later, but it does not change the Observation backlog before the original candidate lands. After land, Fluent turns each follow-up into an Observation.

An Observation becomes corrective Work only when the change is bounded, testable, grounded in an existing Behavior, project instruction, or project Expertise, and requires no unresolved decision. Otherwise it stays an Observation for you to shape through the normal conversation.

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset=".github/assets/how-fluent-improves-dark.png">
    <source media="(prefers-color-scheme: light)" srcset=".github/assets/how-fluent-improves-light.png">
    <img alt="A blocking failure in the current Attempt returns to the Writer. After land, a Learner finding becomes an Observation; a strict corrective gate may turn it into proposed Work that you authorize and explicitly schedule. A separate opt-in post-merge review can start a forward-fix Attempt after a failing or uncertain review. Both follow-up paths stop at a candidate that waits for you." src=".github/assets/how-fluent-improves-light.png" width="100%">
  </picture>
</p>

In the default `propose` mode, the skill shows you the corrective Work and waits. If you authorize it and ask Fluent to run queued Work, the same build loop starts. Authorization does not run or land anything by itself, and the scheduler stops at another ready Merge Candidate for you to inspect and land.

A project can choose `execute` mode to authorize and queue trusted corrective Work automatically within its follow-up limits. The scheduler still runs only when you start it, and every candidate still needs your acceptance before land. The Fluent skill offers this choice before it initializes a new project.

You can also ask Fluent to add a post-merge review when you land a candidate. On a clean fresh land, this separate opt-in schedules a detached Tester and reviewer pass against the landed change. A failing or uncertain review creates and runs a forward-fix Attempt. The Attempt can produce another candidate, but it cannot land it.

In the running example, Fluent might notice that another CI script still parses the human-readable status output. If the project already says machine callers must use versioned JSON, Fluent can propose a bounded correction. Without that rule, it records the finding as an Observation instead.

## How Fluent learns your project

Fluent's learning is project-local, versioned memory. It does not train the underlying model.

After an Attempt produces code and passes review, the Learner sees the complete change and every Tester and reviewer artifact. It can add reusable conventions, constraints, testing patterns, and gotchas to `.fluent/expertise/`, or leave Expertise unchanged when the work taught it nothing durable. It cannot change project source, documentation, or the Observation backlog.

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset=".github/assets/how-fluent-learns-dark.png">
    <source media="(prefers-color-scheme: light)" srcset=".github/assets/how-fluent-learns-light.png">
    <img alt="The completed change plus Tester and reviewer evidence flows into the Learner, which records durable project Expertise in the Merge Candidate. After land, recorded decisions inform planning and relevant Expertise informs Writers and Reviewers. You can inspect and edit it." src=".github/assets/how-fluent-learns-light.png" width="100%">
  </picture>
</p>

Expertise changes are part of the Merge Candidate, so they land with the code that taught them. A Learner failure keeps the candidate from becoming ready. Future planning checks recorded project decisions; Writers and Reviewers load the relevant Expertise. You can edit it directly when it is stale or wrong.

For the JSON status change, Fluent might retain the rule that machine-readable CLI output uses a versioned schema, serializes the existing status model, and does not change the text output. A later Writer starts with that rule, and later Reviewers check it.

Follow-up Work changes what Fluent does next. Expertise changes how Fluent does future work.
