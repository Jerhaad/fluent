-------------------- MODULE ReleaseOwnershipConnection --------------------
EXTENDS Naturals, Sequences
CONSTANTS SettleLegacyChildren, CheckNewBuild, CheckNewStop
VARIABLES owner, lease, state, build, workers, queued, request, ack, result, crashUsed, badReuse, badAdmission, staleStop, nextFresh, operations, published, interrupted, replayInstance, replayOperation, replayResult, replayExpected, released, wa, wb, legacyOwns, legacyStop, aGeneration, aPhase, aHeld, aReceipt, aParentAlive, aStopped, aBadExecution, aBadStop, aBadSignal, aExecutions, bGeneration, bPhase, bHeld, bReceipt, bParentAlive, bStopped, bBadExecution, bBadStop, bBadSignal, bExecutions
svars == <<owner, lease, state, build, workers, queued, request, ack, result, crashUsed, badReuse, badAdmission, staleStop, nextFresh, operations, published, interrupted, replayInstance, replayOperation, replayResult, replayExpected, released>>
avars == <<aGeneration, aPhase, aHeld, aReceipt, aParentAlive, aStopped, aBadExecution, aBadStop, aBadSignal, aExecutions>>
bvars == <<bGeneration, bPhase, bHeld, bReceipt, bParentAlive, bStopped, bBadExecution, bBadStop, bBadSignal, bExecutions>>
vars == <<svars, wa, wb, avars, bvars, legacyOwns, legacyStop>>
WA == INSTANCE Workflow WITH Works <- {"work"}, ReviewOnly <- {},
     MaxFailures <- 1, DeliverInputs <- TRUE, CheckEvidence <- TRUE,
     FenceResume <- TRUE, GuardEffect <- TRUE, TruthfulObservation <- TRUE,
     GuardCleanup <- TRUE, GuardVersion <- TRUE, GuardBaseline <- TRUE,
     GuardLandingBaseline <- TRUE, CanonicalKinds <- {"pass"},
     CorruptPrewrite <- FALSE, TargetKinds <- {"unchanged"},
     GuardReviewRoute <- TRUE, LandingKinds <- {"pass"}, s <- wa
WB == INSTANCE Workflow WITH Works <- {"work"}, ReviewOnly <- {},
     MaxFailures <- 1, DeliverInputs <- TRUE, CheckEvidence <- TRUE,
     FenceResume <- TRUE, GuardEffect <- TRUE, TruthfulObservation <- TRUE,
     GuardCleanup <- TRUE, GuardVersion <- TRUE, GuardBaseline <- TRUE,
     GuardLandingBaseline <- TRUE, CanonicalKinds <- {"pass"},
     CorruptPrewrite <- FALSE, TargetKinds <- {"unchanged"},
     GuardReviewRoute <- TRUE, LandingKinds <- {"pass"}, s <- wb
S == INSTANCE ServiceLifecycle WITH CheckBuild <- CheckNewBuild, BindInstance <- CheckNewStop,
     GuardAdmission <- TRUE, ProtocolCompatible <- TRUE, DrainCapable <- TRUE, ExactReplay <- TRUE, RetainHistory <- TRUE
A == INSTANCE ChildLaunchIdentity WITH RequireReceipt <- TRUE, CheckStop <- TRUE, CheckGeneration <- TRUE,
     generation <- aGeneration, phase <- aPhase, held <- aHeld, receipt <- aReceipt, parentAlive <- aParentAlive, stopped <- aStopped, badExecution <- aBadExecution, badStop <- aBadStop, badSignal <- aBadSignal, executions <- aExecutions
B == INSTANCE ChildLaunchIdentity WITH RequireReceipt <- TRUE, CheckStop <- TRUE, CheckGeneration <- TRUE,
     generation <- bGeneration, phase <- bPhase, held <- bHeld, receipt <- bReceipt, parentAlive <- bParentAlive, stopped <- bStopped, badExecution <- bBadExecution, badStop <- bBadStop, badSignal <- bBadSignal, executions <- bExecutions
\* Begin at the observed running legacy receipts. Legacy launch and its
\* receipt ordering precede this boundary; no current launch guarantee is
\* attributed to the released predecessor.
Init == /\ S!Init /\ legacyOwns = TRUE /\ legacyStop = FALSE
        /\ wa = [w \in {"work"} |-> LET base == WA!InitialWork(w) IN
             [base EXCEPT !.phase = "delivered", !.delivered = base.required,
                          !.serviceOwner = WA!ServiceId(1), !.routeMode = "foreground"]]
        /\ aGeneration = 1
        /\ aPhase = "executing"
        /\ aHeld = TRUE
        /\ aReceipt = 1
        /\ aParentAlive = TRUE
        /\ aStopped = FALSE
        /\ aBadExecution = FALSE
        /\ aBadStop = FALSE
        /\ aBadSignal = FALSE
        /\ aExecutions = 1
        /\ wb = [w \in {"work"} |-> LET base == WB!InitialWork(w) IN
             [base EXCEPT !.phase = "delivered", !.delivered = base.required,
                          !.serviceOwner = WB!ServiceId(1), !.routeMode = "foreground"]]
        /\ bGeneration = 1
        /\ bPhase = "executing"
        /\ bHeld = TRUE
        /\ bReceipt = 1
        /\ bParentAlive = TRUE
        /\ bStopped = FALSE
        /\ bBadExecution = FALSE
        /\ bBadStop = FALSE
        /\ bBadSignal = FALSE
        /\ bExecutions = 1
SettleA == /\ legacyStop /\ A!Settle /\ (WA!Writer("work") \/ WA!Crash("work"))
           /\ UNCHANGED <<svars, wb, bvars, legacyOwns, legacyStop>>
SettleB == /\ legacyStop /\ B!Settle /\ (WB!Writer("work") \/ WB!Crash("work"))
           /\ UNCHANGED <<svars, wa, avars, legacyOwns, legacyStop>>
RequestLegacyStop == /\ legacyOwns /\ ~legacyStop /\ aPhase = "executing" /\ bPhase = "executing"
                     /\ legacyStop' = TRUE /\ UNCHANGED <<svars, wa, wb, avars, bvars, legacyOwns>>
ReleaseLegacy == /\ legacyOwns /\ legacyStop /\ ((~aHeld /\ ~bHeld) \/ ~SettleLegacyChildren)
                 /\ legacyOwns' = FALSE /\ UNCHANGED <<svars, wa, wb, avars, bvars, legacyStop>>
StartCurrent == /\ ~legacyOwns /\ S!Start(1) /\ UNCHANGED <<wa, wb, avars, bvars, legacyOwns, legacyStop>>
ReuseCurrent == /\ (S!Reuse(1) \/ S!Reuse(2)) /\ UNCHANGED <<wa, wb, avars, bvars, legacyOwns, legacyStop>>
StaleStop == /\ S!RequestStop(2) /\ UNCHANGED <<wa, wb, avars, bvars, legacyOwns, legacyStop>>
WrongDrain == /\ S!Acknowledge /\ UNCHANGED <<wa, wb, avars, bvars, legacyOwns, legacyStop>>
Next == SettleA \/ SettleB \/ RequestLegacyStop \/ ReleaseLegacy
        \/ StartCurrent \/ ReuseCurrent \/ StaleStop \/ WrongDrain
Spec == Init /\ [][Next]_vars
NoOverlap == owner # 0 => ~legacyOwns /\ ~aHeld /\ ~bHeld
CurrentIdentity == S!OneOwner /\ S!NoMismatchedReuse /\ S!NoStaleStop
SettledBeforeStart == owner # 0 => aPhase = "settled" /\ bPhase = "settled"
=============================================================================
