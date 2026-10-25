---
name: landlock-rules-union-never-subtract
description: Landlock unions every matching rule, so a nested rule cannot narrow an ancestor's grant; a carve-out must be enumerated into sibling grants at render time, unlike Seatbelt's first-match-wins denies
metadata:
  type: architecture
---

Seatbelt and Landlock resolve overlapping rules in opposite directions, and code that treats the Linux backend as a transliteration of the macOS one is wrong in a way that fails open.

Seatbelt evaluates rules in order and the first match wins, so `common.sb` grants a broad subtree and then denies the sensitive paths inside it. Landlock has no deny rule at all: one layer grants access to a path if *any* rule on that path's ancestry grants it. Adding a narrower rule to take access back is not merely ineffective — it silently widens nothing and leaves the grant standing.

Every withheld path in `src/linux_sandbox.rs` is therefore enumerated rather than denied. `grant_excluding` walks down from a root only as far as an exclusion forces, granting the siblings that lead nowhere near it. Four consequences follow, and each has bitten:

- **The exclusion set applies to every grant, not just the caller's own roots.** A grant on an ancestor covers everything under it, so an exclusion that reaches only the roots the caller named leaves the system, home, and coder grants still covering it: a `~/.claude` grant hands back the project tree an autonomous Claude withholds, and a `/tmp` grant hands back the source checkout a Writer denies. `render` therefore builds one write exclusion set and one read exclusion set and routes every grant — system, temp, home, coder, and the caller's own — through them. A per-call-site carve-out is the same bug in a new place.
- **An excluded path is withheld under both of its spellings.** The kernel resolves the path a process actually opens, so a tree reached through a symlink stays writable when only its logical spelling is excluded. `render` canonicalizes every excluded and re-allowed path, so each carve-out covers the physical tree too.
- **An enumerated directory keeps `Access::List`.** Without it, carving `~/.ssh` out of `$HOME` also stops anything from listing `$HOME`. Listing is what Seatbelt's paired metadata grants allow too: a withheld directory stays discoverable, its file contents do not.
- **Enumeration reads the filesystem, so it is a point-in-time snapshot.** A directory created after rendering falls outside every rule and the sandbox fails closed on it.

Landlock does not mediate `stat`, so Seatbelt's "allow metadata, deny data" pairs have no counterpart: granting nothing already leaves traversal working and contents unreadable.

Two mechanisms are deliberately absent. Landlock's network support (ABI 4) filters TCP by port only and cannot express Seatbelt's "inbound localhost only", so the Linux backend restricts no network access and matches Seatbelt's unrestricted outbound. Rules on `/dev/stdin`, `/dev/stdout`, `/dev/stderr`, and `/dev/fd` are refused by the kernel with `EBADFD` because they are symlinks into `/proc/self/fd`, and they would buy nothing: Landlock mediates opening a path, not writing through an already-open descriptor.

Probe support with a hard requirement, never best-effort. Landlock is commonly compiled into a kernel but left out of the boot `lsm=` list; there the syscall fails with `EOPNOTSUPP` while a best-effort ruleset reports success and enforces nothing. `restrict_self` returning `RulesetStatus::NotEnforced` fails the launch rather than running a coder unconfined.

A test that asserts a forbidden action failed proves nothing on such a host — the launcher refused to start, so nothing was confined. `tests/linux_sandbox.rs` names which of the two outcomes each assertion expects. A test that only asserts that no *rule names* a withheld path is worse than vacuous: a rule on an ancestor grants the path anyway, so the assertions have to resolve through the ancestry the way the kernel does, or run a confined write and watch it fail.

Related: [[canonicalize-confinement-paths]], [[strict-sandbox-capability-selection]], [[sandbox-tests-assert-invariant-in-both-host-branches]]
