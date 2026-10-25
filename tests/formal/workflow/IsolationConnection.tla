------------------------ MODULE IsolationConnection ------------------------
EXTENDS Naturals, Sequences
CONSTANT ContainProjectFailure
VARIABLES wf, readable, running, cursor, completed, blocked
ivars == <<readable, running, cursor, completed, blocked>>
vars == <<wf, ivars>>
W == INSTANCE Workflow WITH Works <- {1,2}, ReviewOnly <- {},
     MaxFailures <- 0, DeliverInputs <- TRUE, CheckEvidence <- TRUE,
     FenceResume <- TRUE, GuardEffect <- TRUE, TruthfulObservation <- TRUE,
     GuardCleanup <- TRUE, GuardVersion <- TRUE, GuardBaseline <- TRUE,
     GuardLandingBaseline <- TRUE, CanonicalKinds <- {"pass"},
     CorruptPrewrite <- FALSE, TargetKinds <- {"unchanged"},
     GuardReviewRoute <- TRUE, LandingKinds <- {"pass"}, s <- wf
I == INSTANCE ServiceIsolation WITH ContainFailure <- ContainProjectFailure
Init == /\ I!Init
        /\ wf = [w \in {1,2} |-> [W!InitialWork(w) EXCEPT
                    !.phase = "authorized", !.routeMode = "queued", !.queued = TRUE]]
Poll == /\ I!PollProject
        /\ IF readable[cursor] /\ cursor \notin completed
              THEN W!Dispatch(cursor) ELSE UNCHANGED wf
NextPoll == /\ I!NextPoll /\ UNCHANGED wf
Next == Poll \/ NextPoll
Spec == Init /\ [][Next]_vars /\ WF_vars(Poll) /\ WF_vars(NextPoll)
\* The failed project remains failed throughout this bounded execution.
\* Progress cannot be discharged by assuming repair or service restart.
HealthyProgress == <> (wf[2].phase = "dispatched")
MappedDispatch == \A w \in {1,2}: (w \in completed) = (wf[w].phase = "dispatched")
FailedProjectRetained == wf[1].queued /\ wf[1].serviceOwner = W!Unowned
=============================================================================
