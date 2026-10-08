------------------------- MODULE GenerationFence --------------------------
EXTENDS Naturals, Sequences

CONSTANT CheckGeneration
Actors == {"starter", "settler"}
VARIABLES generation, queue, pc, saved, accepted
vars == <<generation, queue, pc, saved, accepted>>

Init ==
    /\ generation = 1
    /\ queue = "Claimed"
    /\ pc = [a \in Actors |-> "Read"]
    /\ saved = [a \in Actors |-> 0]
    /\ accepted = <<>>

Read(a) ==
    /\ pc[a] = "Read"
    /\ saved' = [saved EXCEPT ![a] = generation]
    /\ pc' = [pc EXCEPT ![a] = "Commit"]
    /\ UNCHANGED <<generation, queue, accepted>>

Eligible(a) ==
    /\ (IF a = "starter" THEN queue = "Claimed" ELSE queue \in {"Claimed", "Running"})
    /\ (~CheckGeneration \/ saved[a] = generation)

Commit(a) ==
    /\ pc[a] = "Commit"
    /\ pc' = [pc EXCEPT ![a] = "Done"]
    /\ IF Eligible(a)
          THEN /\ accepted' = Append(accepted, [actor |-> a, observed |-> saved[a], current |-> generation])
               /\ generation' = generation + 1
               /\ queue' = IF a = "starter" THEN "Running" ELSE "Done"
          ELSE UNCHANGED <<generation, queue, accepted>>
    /\ UNCHANGED saved

Next == \E a \in Actors : Read(a) \/ Commit(a)
Spec == Init /\ [][Next]_vars /\ (\A a \in Actors : WF_vars(Read(a)) /\ WF_vars(Commit(a)))
TypeOK ==
    /\ generation \in 1..3
    /\ queue \in {"Claimed", "Running", "Done"}
    /\ pc \in [Actors -> {"Read", "Commit", "Done"}]
    /\ saved \in [Actors -> 0..3]
    /\ Len(accepted) <= 2
NoStaleCommit == \A i \in 1..Len(accepted) : accepted[i].observed = accepted[i].current
EventuallyRespond == \A a \in Actors : <>(pc[a] = "Done")
=============================================================================
