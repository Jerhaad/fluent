----------------------- MODULE QueueDispatchConnection -----------------------
EXTENDS Naturals, FiniteSets

\* A joint boundary model: Workflow.Dispatch occurs at QueueHandoff.MarkRunning.
\* Claim, lease acquisition, and Attempt binding are visible stuttering steps
\* of the workflow. The two Jobs compete for one actor and one service owner.
CONSTANTS Jobs, Actor, NoActor, HonorDispatchLease, HonorCoordinatorLease,
          MaxCrashes, PreserveBoundAttempt, HonorRecoveryLease
ASSUME /\ Cardinality(Jobs) = 2 /\ Actor # NoActor
       /\ HonorDispatchLease \in BOOLEAN
       /\ HonorCoordinatorLease \in BOOLEAN

VARIABLES wf, qqueue, qgeneration, qattempts, qdone, qlease, qpc,
          qwork, qtoken, qcrashes, bound, coordinator
vars == <<wf, qqueue, qgeneration, qattempts, qdone, qlease,
          qpc, qwork, qtoken, qcrashes, bound, coordinator>>
qvars == <<qqueue, qgeneration, qattempts, qdone, qlease,
           qpc, qwork, qtoken, qcrashes>>

W == INSTANCE Workflow WITH Works <- Jobs, ReviewOnly <- {},
     MaxFailures <- MaxCrashes, DeliverInputs <- TRUE, CheckEvidence <- TRUE,
     FenceResume <- TRUE, GuardEffect <- TRUE, TruthfulObservation <- TRUE,
     GuardCleanup <- TRUE, GuardVersion <- TRUE, GuardBaseline <- TRUE,
     GuardLandingBaseline <- TRUE, CanonicalKinds <- {"pass"},
     CorruptPrewrite <- FALSE, TargetKinds <- {"unchanged"},
     GuardReviewRoute <- TRUE, LandingKinds <- {"pass"}, s <- wf
Q == INSTANCE QueueHandoff WITH Jobs <- Jobs, Actors <- {Actor},
     None <- NoActor, MaxCrashes <- MaxCrashes, MaxGeneration <- 10,
     RecoverClaims <- TRUE, HonorLeases <- HonorRecoveryLease, PreserveAttempt <- PreserveBoundAttempt,
     queue <- qqueue, generation <- qgeneration, attempts <- qattempts,
     attemptDone <- qdone, lease <- qlease, pc <- qpc, work <- qwork,
     token <- qtoken, crashes <- qcrashes

Init == /\ W!Init /\ Q!Init
        /\ bound = [j \in Jobs |-> <<"unbound", j>>]
        /\ coordinator = NoActor

Elect == /\ coordinator = NoActor
         /\ coordinator' = Actor
         /\ UNCHANGED <<wf, qvars, bound>>

Prepare(j) ==
    /\ (W!Intake(j) \/ W!AttemptCreate(j) \/ W!Authorize(j)
        \/ W!Enqueue(j))
    /\ UNCHANGED <<qvars, bound, coordinator>>

\* The queue lock serializes the durable claim. The saved Attempt identity is
\* chosen before that write; EnsureAttempt must reuse exactly that identity.
Claim(j) ==
    /\ coordinator = Actor
    /\ wf[j].phase = "authorized" /\ wf[j].routeMode = "queued"
    /\ \A other \in Jobs \ {j}: wf[other].serviceOwner = W!Unowned
    /\ Q!Claim(Actor, j)
    /\ bound' = [bound EXCEPT ![j] = wf[j].attempt]
    /\ UNCHANGED <<wf, coordinator>>
UnleasedClaim(j) ==
    /\ ~HonorCoordinatorLease /\ coordinator = NoActor
    /\ wf[j].phase = "authorized" /\ wf[j].routeMode = "queued"
    /\ Q!Claim(Actor, j)
    /\ bound' = [bound EXCEPT ![j] = wf[j].attempt]
    /\ UNCHANGED <<wf, coordinator>>
Acquire == /\ Q!AcquireLease(Actor)
           /\ UNCHANGED <<wf, bound, coordinator>>
Bind == /\ Q!EnsureAttempt(Actor)
        /\ bound[qwork[Actor]] = wf[qwork[Actor]].attempt
        /\ UNCHANGED <<wf, bound, coordinator>>

Dispatch ==
    /\ qpc[Actor] = "Bound"
    /\ qlease[qwork[Actor]] = Actor
    /\ bound[qwork[Actor]] = wf[qwork[Actor]].attempt
    /\ Q!MarkRunning(Actor)
    /\ W!Dispatch(qwork[Actor])
    /\ UNCHANGED <<bound, coordinator>>

Deliver == /\ \E j \in Jobs: W!Deliver(j)
           /\ UNCHANGED <<qvars, bound, coordinator>>
Finish == /\ qpc[Actor] = "Running"
          /\ W!Writer(qwork[Actor])
          /\ Q!CompleteAttempt(Actor)
          /\ UNCHANGED <<bound, coordinator>>
Settle == /\ Q!Settle(Actor)
          /\ UNCHANGED <<wf, bound, coordinator>>

\* Named defect control: dispatching after the claim but before the held
\* Attempt lease makes the abstract workflow claim execution too early.
EarlyDispatch ==
    /\ ~HonorDispatchLease /\ qpc[Actor] = "Claimed"
    /\ W!Dispatch(qwork[Actor])
    /\ UNCHANGED <<qvars, bound, coordinator>>

\* A process crash releases the OS lease but preserves the durable binding.
Crash == /\ qpc[Actor] \in {"Claimed", "Leased", "Bound", "Running"}
         /\ W!Crash(qwork[Actor]) /\ Q!Crash(Actor)
         /\ coordinator' = NoActor
         /\ UNCHANGED bound
Restart == /\ Q!Restart(Actor)
           /\ UNCHANGED <<wf, bound, coordinator>>
Recover(j) == /\ wf[j].failed /\ Q!Recover(j) /\ W!Recover(j)
              /\ UNCHANGED <<bound, coordinator>>
\* The negative control models a recovery poll ignoring a live owner.
RecoverLive(j) == /\ ~HonorRecoveryLease /\ ~wf[j].failed
                  /\ Q!Recover(j)
                  /\ UNCHANGED <<wf, bound, coordinator>>

Next == \/ Elect
        \/ \E j \in Jobs: Prepare(j) \/ Claim(j) \/ UnleasedClaim(j)
        \/ Acquire \/ Bind \/ Dispatch \/ Deliver \/ Finish \/ Settle
        \/ EarlyDispatch \/ Crash \/ Restart
        \/ \E j \in Jobs: Recover(j) \/ RecoverLive(j)
Fairness == /\ WF_vars(Elect) /\ WF_vars(Restart)
            /\ WF_vars(Acquire) /\ WF_vars(Bind) /\ WF_vars(Dispatch)
            /\ WF_vars(Deliver) /\ WF_vars(Finish) /\ WF_vars(Settle)
            /\ \A j \in Jobs: WF_vars(Claim(j)) /\ WF_vars(Recover(j))
Spec == Init /\ [][Next]_vars /\ Fairness

\* These establish the assumptions used by the stable refinement mapping.
SerializedClaimFrame ==
    \A j \in Jobs: qpc[Actor] = "Claimed" /\ qwork[Actor] = j =>
        /\ wf[j].routeMode = "queued"
        /\ wf[j].dispatch = W!DispatchId(j)
        /\ bound[j] = wf[j].attempt
ClaimRequiresCoordinator ==
    qpc[Actor] \notin {"Idle", "Down"} => coordinator = Actor
CurrentLease ==
    \A j \in Jobs: qwork[Actor] = j /\
        qpc[Actor] \in {"Leased", "Bound", "Running", "Finished"} =>
        /\ qlease[j] = Actor
        /\ qtoken[Actor] = qgeneration[j]
CurrentProjectOwner ==
    qpc[Actor] \in {"Claimed", "Leased", "Bound"} =>
        \A j \in Jobs: wf[j].serviceOwner = W!Unowned

\* At a stable dispatch boundary, map the Work, Attempt, dispatch and owner
\* identities to the same job. Intermediate claim steps may stutter only
\* while Workflow has not entered its dispatched phase.
MappedDispatch ==
    \A j \in Jobs: wf[j].phase \in {"dispatched", "delivered"} /\ ~wf[j].failed =>
        /\ qwork[Actor] = j
        /\ qpc[Actor] = "Running"
        /\ qqueue[j] = "Running"
        /\ qlease[j] = Actor
        /\ bound[j] = wf[j].attempt
        /\ wf[j].dispatch = W!DispatchId(j)
        /\ wf[j].serviceOwner = W!ServiceId(wf[j].generation)
        /\ qtoken[Actor] = qgeneration[j]

CompetingWorkPreserved ==
    \A j \in Jobs: qqueue[j] = "Queued" /\ qgeneration[j] = 0 =>
        /\ qattempts[j] = 0
        /\ qlease[j] = NoActor
        /\ wf[j].serviceOwner = W!Unowned

SameAttempt == \A j \in Jobs:
    /\ qattempts[j] <= 1
    /\ qgeneration[j] > 0 => bound[j] = wf[j].attempt
RecoveryReleasesOwner == \A j \in Jobs: wf[j].failed =>
    /\ qlease[j] = NoActor
    /\ wf[j].serviceOwner = W!Unowned
RecoveryCanDispatch == \A j \in Jobs: wf[j].failed ~> (wf[j].phase = "written")
=============================================================================
