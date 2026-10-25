-------------------------- MODULE ServiceConnection --------------------------
EXTENDS Naturals, FiniteSets
CONSTANTS CheckCurrentBuild, CheckCurrentStop, SettleBeforeRelease
VARIABLES wf, owner, lease, state, build, workers, queued, request, ack, result, crashUsed, badReuse, badAdmission, staleStop, nextFresh, operations, published, interrupted, replayInstance, replayOperation, replayResult, replayExpected, released,
          cGeneration, cPhase, cHeld, cReceipt, cParentAlive, cStopped, cBadExecution, cBadStop, cBadSignal, cExecutions, childOwner
svars == <<owner, lease, state, build, workers, queued, request, ack, result, crashUsed, badReuse, badAdmission, staleStop, nextFresh, operations, published, interrupted, replayInstance, replayOperation, replayResult, replayExpected, released>>
cvars == <<cGeneration, cPhase, cHeld, cReceipt, cParentAlive, cStopped, cBadExecution, cBadStop, cBadSignal, cExecutions>>
vars == <<wf, svars, cvars, childOwner>>
W == INSTANCE Workflow WITH Works <- {"work"}, ReviewOnly <- {},
     MaxFailures <- 2, DeliverInputs <- TRUE, CheckEvidence <- TRUE,
     FenceResume <- TRUE, GuardEffect <- TRUE, TruthfulObservation <- TRUE,
     GuardCleanup <- TRUE, GuardVersion <- TRUE, GuardBaseline <- TRUE,
     GuardLandingBaseline <- TRUE, CanonicalKinds <- {"pass"},
     CorruptPrewrite <- FALSE, TargetKinds <- {"unchanged"},
     GuardReviewRoute <- TRUE, LandingKinds <- {"pass"}, s <- wf


S == INSTANCE ServiceLifecycle WITH CheckBuild <- CheckCurrentBuild,
     BindInstance <- CheckCurrentStop, GuardAdmission <- TRUE,
     ProtocolCompatible <- TRUE, DrainCapable <- TRUE,
     ExactReplay <- TRUE, RetainHistory <- TRUE
C == INSTANCE ChildLaunchIdentity WITH RequireReceipt <- TRUE,
     CheckStop <- TRUE, CheckGeneration <- TRUE,
     generation <- cGeneration,
     phase <- cPhase,
     held <- cHeld,
     receipt <- cReceipt,
     parentAlive <- cParentAlive,
     stopped <- cStopped,
     badExecution <- cBadExecution,
     badStop <- cBadStop,
     badSignal <- cBadSignal,
     executions <- cExecutions
Init == /\ W!Init /\ S!Init /\ C!Init /\ childOwner = 0
Prepare == /\ (W!Intake("work") \/ W!AttemptCreate("work") \/ W!Authorize("work")
                \/ W!ChooseForeground("work"))
           /\ UNCHANGED <<svars, cvars, childOwner>>
Start == /\ \E i \in S!Instances: S!Start(i)
         /\ UNCHANGED <<wf, cvars, childOwner>>
Reuse == /\ \E i \in S!Instances: S!Reuse(i)
         /\ UNCHANGED <<wf, cvars, childOwner>>
Submit == /\ \E p \in S!Projects: S!Submit(p)
          /\ UNCHANGED <<wf, cvars, childOwner>>
Claim == /\ S!Claim("A") /\ W!Dispatch("work") /\ C!Reserve
         /\ childOwner' = owner
Create == /\ C!Create /\ UNCHANGED <<wf, svars, childOwner>>
Receipt == /\ C!ChildReceipt /\ UNCHANGED <<wf, svars, childOwner>>
Execute == /\ C!Execute /\ W!Deliver("work") /\ UNCHANGED <<svars, childOwner>>
RequestStop == /\ \E i \in S!Instances: S!RequestStop(i)
               /\ UNCHANGED <<wf, cvars, childOwner>>
Drain == /\ S!Acknowledge /\ UNCHANGED <<wf, cvars, childOwner>>
Settle == /\ childOwner = owner /\ S!Settle("A") /\ C!Settle /\ W!Writer("work")
          /\ UNCHANGED childOwner
\* Defect control: a completed thread count alone is not child settlement.
ClearAccounting == /\ ~SettleBeforeRelease /\ S!Settle("A")
                   /\ UNCHANGED <<wf, cvars, childOwner>>
Publish == /\ S!Publish /\ UNCHANGED <<wf, cvars, childOwner>>
Release == /\ S!Cleanup /\ UNCHANGED <<wf, cvars, childOwner>>
Crash == /\ S!Crash
         /\ IF cParentAlive /\ childOwner = owner
               THEN /\ C!Crash /\ W!Crash("work")
               ELSE UNCHANGED <<wf, cvars>>
         /\ UNCHANGED childOwner
RecoverChild == /\ C!Recover /\ UNCHANGED <<wf, svars, childOwner>>
Replay == /\ \E i \in S!Instances: S!Replay(i)
          /\ UNCHANGED <<wf, cvars, childOwner>>
Next == Prepare \/ Start \/ Reuse \/ Submit \/ Claim \/ Create \/ Receipt \/ Execute
        \/ RequestStop \/ Drain \/ Settle \/ ClearAccounting \/ Publish \/ Release
        \/ Crash \/ RecoverChild \/ Replay
Spec == Init /\ [][Next]_vars
OwnerBound == /\ S!OneOwner /\ S!NoMismatchedReuse /\ S!NoStaleStop
ChildBeforePublication == \A i \in S!Instances:
    published[i] /\ childOwner = i => ~cHeld
CurrentChildAccounting == workers["A"] = 1 =>
    /\ childOwner = owner
    /\ wf["work"].serviceOwner = W!ServiceId(wf["work"].generation)
    /\ wf["work"].phase \in {"dispatched", "delivered"}
RetainedHistory == /\ S!RetainedResult /\ S!ReplayStable /\ S!NoEarlyRelease
QueuedProjectRetained == workers["B"] = 0
=============================================================================
