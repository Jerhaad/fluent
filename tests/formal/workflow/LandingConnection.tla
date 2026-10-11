------------------------ MODULE LandingConnection ------------------------
EXTENDS Naturals, Sequences
CONSTANTS BindCurrentIntent, ReconcileFirst, PreserveIndependent, RetainPending, ReportTruthfully
VARIABLES wf, owner, crashes, failures, intent, target, result, provenance, cleaned,
          stopped, ioFailed, effects, independent, badUnbound, badReplay
lvars == <<owner, crashes, failures, intent, target, result, provenance, cleaned,
           stopped, ioFailed, effects, independent, badUnbound, badReplay>>
vars == <<wf, lvars>>
W == INSTANCE Workflow WITH Works <- {"work"}, ReviewOnly <- {},
     MaxFailures <- 2, DeliverInputs <- TRUE, CheckEvidence <- TRUE,
     FenceResume <- TRUE, GuardEffect <- ReconcileFirst, TruthfulObservation <- ReportTruthfully,
     GuardCleanup <- RetainPending, GuardVersion <- TRUE, GuardBaseline <- TRUE,
     GuardLandingBaseline <- TRUE, CanonicalKinds <- {"pass"},
     CorruptPrewrite <- FALSE, TargetKinds <- {"unchanged"},
     GuardReviewRoute <- TRUE, LandingKinds <- {"pass"}, s <- wf
L == INSTANCE LandingPublication WITH Candidates <- {"candidate"},
     Actors <- {"writer", "retry"}, BindIntent <- BindCurrentIntent,
     EarlyReconcile <- ReconcileFirst, ReplayProtection <- PreserveIndependent
\* The public ready snapshot establishes this starting metadata boundary;
\* this connection checks the subsequent publication protocol, not readiness.
Init == /\ L!Init
        /\ wf = [w \in {"work"} |-> LET base == W!InitialWork(w) IN
             [base EXCEPT !.phase = "verified", !.delivered = base.required,
                          !.rawEvidence = {W!EvidenceId(w,1)},
                          !.credit = {W!EvidenceId(w,1)},
                          !.acceptedCandidate = W!CandidateId(w)]]
Intent == /\ L!PublishIntent /\ W!Intent("work")
Effect == /\ L!GitEffect
          /\ IF intent = "candidate" THEN W!GitEffect("work") ELSE UNCHANGED wf
Publish == /\ L!PublishResult /\ W!PublishResult("work")
Crash == /\ L!Crash /\ W!Crash("work")
Recover == /\ W!Recover("work") /\ UNCHANGED lvars
FailWrite == /\ L!FailWrite /\ W!FailWrite("work")
RecoverIO == /\ L!RecoverIO /\ W!RecoverIO("work")
StartFollowUp == /\ W!FollowUpStart("work") /\ UNCHANGED lvars
FollowUp == /\ ~ioFailed /\ W!FollowUp("work") /\ UNCHANGED lvars
Advance == /\ L!Advance /\ UNCHANGED wf
IndependentWrite == /\ L!IndependentWrite /\ UNCHANGED wf
\* After result publication a successful retry stutters at the workflow
\* level. The detailed invariant must independently forbid another effect.
Replay == /\ ~wf["work"].failed /\ L!Retry /\ UNCHANGED wf
Cleanup == /\ W!Cleanup("work") /\ L!Cleanup
Observe == /\ W!Observe("work") /\ UNCHANGED lvars
Next == Observe \/ Intent \/ Effect \/ Publish \/ Crash \/ Recover \/ FailWrite \/ RecoverIO
        \/ StartFollowUp \/ FollowUp \/ Advance \/ IndependentWrite \/ Replay \/ Cleanup
Spec == Init /\ [][Next]_vars
NoUnboundEffect == L!NoUnboundEffect
OneEffect == /\ L!OneEffect /\ effects = wf["work"].effects
RetainIndependent == L!RetainIndependent
MappedPublication == /\ (result = "candidate") = wf["work"].published
                     /\ (intent = "candidate") = (wf["work"].pending \/ wf["work"].published)
                     /\ ioFailed = wf["work"].ioFailed
                     /\ cleaned = wf["work"].cleaned
CleanupSafe == W!CleanupSafe
ObservationSafe == W!ObservationSafe
=============================================================================
