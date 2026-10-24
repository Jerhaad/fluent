---- MODULE GenerationFence ----
EXTENDS Naturals, TLC
CONSTANTS GenerationFenceEnabled, IdentityFenceEnabled
VARIABLES generation, identity, staleGeneration, staleIdentity, status, staleChanged
vars == <<generation, identity, staleGeneration, staleIdentity, status, staleChanged>>
Init == /\ generation = 0
        /\ identity = 0
        /\ staleGeneration = 0
        /\ staleIdentity = 0
        /\ status = "claimed"
        /\ staleChanged = FALSE
AdvanceGeneration == /\ generation = 0
                     /\ generation' = 1
                     /\ status' = "running"
                     /\ UNCHANGED <<identity, staleGeneration, staleIdentity, staleChanged>>
ReplaceIdentity == /\ identity = 0
                   /\ identity' = 1
                   /\ status' = "running"
                   /\ UNCHANGED <<generation, staleGeneration, staleIdentity, staleChanged>>
StaleRecover == /\ status = "running"
                /\ (~GenerationFenceEnabled \/ staleGeneration = generation)
                /\ (~IdentityFenceEnabled \/ staleIdentity = identity)
                /\ status' = "queued"
                /\ staleChanged' = TRUE
                /\ UNCHANGED <<generation, identity, staleGeneration, staleIdentity>>
Next == AdvanceGeneration \/ ReplaceIdentity \/ StaleRecover
Spec == Init /\ [][Next]_vars
StaleOperationRejected == ~staleChanged
====
