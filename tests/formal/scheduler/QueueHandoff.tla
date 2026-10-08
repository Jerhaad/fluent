--------------------------- MODULE QueueHandoff ---------------------------
EXTENDS Naturals, FiniteSets

CONSTANTS Jobs, Actors, None, MaxCrashes, MaxGeneration,
          RecoverClaims, HonorLeases, PreserveAttempt

VARIABLES queue, generation, attempts, attemptDone, lease, pc, work, token,
          crashes

vars == <<queue, generation, attempts, attemptDone, lease, pc, work, token,
          crashes>>

ActivePhases == {"Leased", "Bound", "Running", "Finished"}

Init ==
    /\ queue = [j \in Jobs |-> "Queued"]
    /\ generation = [j \in Jobs |-> 0]
    /\ attempts = [j \in Jobs |-> 0]
    /\ attemptDone = [j \in Jobs |-> FALSE]
    /\ lease = [j \in Jobs |-> None]
    /\ pc = [a \in Actors |-> "Idle"]
    /\ work = [a \in Actors |-> None]
    /\ token = [a \in Actors |-> 0]
    /\ crashes = 0

TypeOK ==
    /\ queue \in [Jobs -> {"Queued", "Claimed", "Running", "Done"}]
    /\ generation \in [Jobs -> 0..MaxGeneration]
    /\ attempts \in [Jobs -> 0..2]
    /\ attemptDone \in [Jobs -> BOOLEAN]
    /\ lease \in [Jobs -> Actors \cup {None}]
    /\ pc \in [Actors -> {"Idle", "Claimed", "Leased", "Bound", "Running", "Finished", "Down"}]
    /\ work \in [Actors -> Jobs \cup {None}]
    /\ token \in [Actors -> 0..MaxGeneration]
    /\ crashes \in 0..MaxCrashes

OneAttempt == \A j \in Jobs : attempts[j] <= 1

OneOwner ==
    \A j \in Jobs :
        Cardinality({a \in Actors : work[a] = j /\ pc[a] \in ActivePhases}) <= 1

OwnerTokenCurrent ==
    \A a \in Actors :
        pc[a] \in ActivePhases =>
            /\ lease[work[a]] = a
            /\ token[a] = generation[work[a]]

Claim(a, j) ==
    /\ pc[a] = "Idle"
    /\ queue[j] = "Queued"
    /\ generation[j] < MaxGeneration
    \* The service's synchronous claim/lease frame cannot overlap another poll.
    /\ ~\E other \in Actors : pc[other] = "Claimed"
    /\ queue' = [queue EXCEPT ![j] = "Claimed"]
    /\ generation' = [generation EXCEPT ![j] = @ + 1]
    /\ pc' = [pc EXCEPT ![a] = "Claimed"]
    /\ work' = [work EXCEPT ![a] = j]
    /\ token' = [token EXCEPT ![a] = generation[j] + 1]
    /\ UNCHANGED <<attempts, attemptDone, lease, crashes>>

AcquireLease(a) ==
    /\ pc[a] = "Claimed"
    /\ lease[work[a]] = None
    /\ lease' = [lease EXCEPT ![work[a]] = a]
    /\ pc' = [pc EXCEPT ![a] = "Leased"]
    /\ UNCHANGED <<queue, generation, attempts, attemptDone, work, token, crashes>>

EnsureAttempt(a) ==
    /\ pc[a] = "Leased"
    /\ attempts' = [attempts EXCEPT ![work[a]] =
           IF PreserveAttempt /\ @ > 0 THEN @ ELSE @ + 1]
    /\ pc' = [pc EXCEPT ![a] = "Bound"]
    /\ UNCHANGED <<queue, generation, attemptDone, lease, work, token, crashes>>

MarkRunning(a) ==
    /\ pc[a] = "Bound"
    /\ queue[work[a]] = "Claimed"
    /\ token[a] = generation[work[a]]
    /\ generation[work[a]] < MaxGeneration
    /\ queue' = [queue EXCEPT ![work[a]] = "Running"]
    /\ generation' = [generation EXCEPT ![work[a]] = @ + 1]
    /\ token' = [token EXCEPT ![a] = @ + 1]
    /\ pc' = [pc EXCEPT ![a] = "Running"]
    /\ UNCHANGED <<attempts, attemptDone, lease, work, crashes>>

CompleteAttempt(a) ==
    /\ pc[a] = "Running"
    /\ attemptDone' = [attemptDone EXCEPT ![work[a]] = TRUE]
    /\ pc' = [pc EXCEPT ![a] = "Finished"]
    /\ UNCHANGED <<queue, generation, attempts, lease, work, token, crashes>>

Settle(a) ==
    /\ pc[a] = "Finished"
    /\ token[a] = generation[work[a]]
    /\ generation[work[a]] < MaxGeneration
    /\ queue' = [queue EXCEPT ![work[a]] = "Done"]
    /\ generation' = [generation EXCEPT ![work[a]] = @ + 1]
    /\ lease' = [lease EXCEPT ![work[a]] = None]
    /\ pc' = [pc EXCEPT ![a] = "Idle"]
    /\ work' = [work EXCEPT ![a] = None]
    /\ token' = [token EXCEPT ![a] = 0]
    /\ UNCHANGED <<attempts, attemptDone, crashes>>

Crash(a) ==
    /\ pc[a] \notin {"Idle", "Down"}
    /\ crashes < MaxCrashes
    /\ lease' = [j \in Jobs |-> IF lease[j] = a THEN None ELSE lease[j]]
    /\ pc' = [pc EXCEPT ![a] = "Down"]
    /\ work' = [work EXCEPT ![a] = None]
    /\ token' = [token EXCEPT ![a] = 0]
    /\ crashes' = crashes + 1
    /\ UNCHANGED <<queue, generation, attempts, attemptDone>>

CrashAll ==
    /\ \E a \in Actors : pc[a] \notin {"Idle", "Down"}
    /\ crashes < MaxCrashes
    /\ lease' = [j \in Jobs |-> None]
    /\ pc' = [a \in Actors |-> "Down"]
    /\ work' = [a \in Actors |-> None]
    /\ token' = [a \in Actors |-> 0]
    /\ crashes' = crashes + 1
    /\ UNCHANGED <<queue, generation, attempts, attemptDone>>

Restart(a) ==
    /\ pc[a] = "Down"
    /\ pc' = [pc EXCEPT ![a] = "Idle"]
    /\ UNCHANGED <<queue, generation, attempts, attemptDone, lease, work, token, crashes>>

Recover(j) ==
    /\ RecoverClaims
    /\ queue[j] \in {"Claimed", "Running"}
    /\ ~\E a \in Actors : pc[a] = "Claimed"
    /\ (~HonorLeases \/ lease[j] = None)
    /\ generation[j] < MaxGeneration
    /\ queue' = [queue EXCEPT ![j] = IF attemptDone[j] THEN "Done" ELSE "Queued"]
    /\ generation' = [generation EXCEPT ![j] = @ + 1]
    /\ UNCHANGED <<attempts, attemptDone, lease, pc, work, token, crashes>>

Next ==
    \/ \E a \in Actors, j \in Jobs : Claim(a, j)
    \/ \E a \in Actors : AcquireLease(a) \/ EnsureAttempt(a) \/ MarkRunning(a)
                             \/ CompleteAttempt(a) \/ Settle(a) \/ Crash(a) \/ Restart(a)
    \/ \E j \in Jobs : Recover(j)
    \/ CrashAll

Fairness ==
    /\ \A a \in Actors, j \in Jobs : WF_vars(Claim(a, j))
    /\ \A a \in Actors :
           /\ WF_vars(AcquireLease(a))
           /\ WF_vars(EnsureAttempt(a))
           /\ WF_vars(MarkRunning(a))
           /\ WF_vars(CompleteAttempt(a))
           /\ WF_vars(Settle(a))
           /\ WF_vars(Restart(a))
    /\ \A j \in Jobs : WF_vars(Recover(j))

Spec == Init /\ [][Next]_vars /\ Fairness
EventuallyDone == \A j \in Jobs : <>(queue[j] = "Done")
=============================================================================
