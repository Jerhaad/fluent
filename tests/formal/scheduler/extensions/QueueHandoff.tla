---- MODULE QueueHandoff ----
EXTENDS Naturals, TLC
CONSTANTS RecoveryEnabled, DuplicateEnabled
VARIABLES phase, owner, attempt, outcome, generation, crashes
vars == <<phase, owner, attempt, outcome, generation, crashes>>

Init == /\ phase = "queued"
        /\ owner = "none"
        /\ attempt = 0
        /\ outcome = "none"
        /\ generation = 0
        /\ crashes = 0

Claim == /\ phase = "queued"
         /\ generation < 7
         /\ phase' = "claimed"
         /\ owner' = "none"
         /\ generation' = generation + 1
         /\ UNCHANGED <<attempt, outcome, crashes>>
Acquire == /\ phase = "claimed"
           /\ owner = "none"
           /\ owner' = "live"
           /\ UNCHANGED <<phase, attempt, outcome, generation, crashes>>
CreateAttempt == /\ phase = "claimed"
                 /\ owner = "live"
                 /\ attempt = 0
                 /\ attempt' = 1
                 /\ UNCHANGED <<phase, owner, outcome, generation, crashes>>
Run == /\ phase = "claimed"
       /\ owner = "live"
       /\ attempt = 1
       /\ phase' = "running"
       /\ UNCHANGED <<owner, attempt, outcome, generation, crashes>>
DurableFinish == /\ phase = "running"
                 /\ owner = "live"
                 /\ outcome = "none"
                 /\ outcome' = "done"
                 /\ UNCHANGED <<phase, owner, attempt, generation, crashes>>
Publish == /\ phase = "running"
           /\ owner = "live"
           /\ outcome = "done"
           /\ phase' = "done"
           /\ owner' = "none"
           /\ UNCHANGED <<attempt, outcome, generation, crashes>>
Crash == /\ phase \in {"claimed", "running"}
         /\ owner = "live"
         /\ crashes = 0
         /\ owner' = "dead"
         /\ crashes' = 1
         /\ UNCHANGED <<phase, attempt, outcome, generation>>
Recover == /\ RecoveryEnabled
           /\ phase \in {"claimed", "running"}
           /\ owner \in {"none", "dead"}
           /\ generation < 8
           /\ phase' = IF outcome = "done" THEN "done" ELSE "queued"
           /\ owner' = "none"
           /\ generation' = generation + 1
           /\ UNCHANGED <<attempt, outcome, crashes>>
Duplicate == /\ DuplicateEnabled
             /\ phase = "running"
             /\ attempt = 1
             /\ attempt' = 2
             /\ UNCHANGED <<phase, owner, outcome, generation, crashes>>
Next == Claim \/ Acquire \/ CreateAttempt \/ Run \/ DurableFinish \/ Publish \/ Crash \/ Recover \/ Duplicate
Spec == Init /\ [][Next]_vars
OneAttempt == attempt <= 1
Recoverable == (phase \in {"claimed", "running"} /\ owner \in {"none", "dead"}) => ENABLED Recover
====
