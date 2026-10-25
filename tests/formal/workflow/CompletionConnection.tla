----------------------- MODULE CompletionConnection -----------------------
EXTENDS Naturals, Sequences
CONSTANTS FinishPublication, PreserveHistory, IdempotentReplay
VARIABLES wf, prefix, published, interrupted, crashUsed, taskCount, history
vars == <<wf, prefix, published, interrupted, crashUsed, taskCount, history>>
W == INSTANCE Workflow WITH Works <- {"work"}, ReviewOnly <- {},
     MaxFailures <- 1, DeliverInputs <- TRUE, CheckEvidence <- TRUE,
     FenceResume <- TRUE, GuardEffect <- TRUE, TruthfulObservation <- TRUE,
     GuardCleanup <- TRUE, GuardVersion <- TRUE, GuardBaseline <- TRUE,
     GuardLandingBaseline <- TRUE, CanonicalKinds <- {"pass"},
     CorruptPrewrite <- FALSE, TargetKinds <- {"unchanged"},
     GuardReviewRoute <- TRUE, LandingKinds <- {"pass"}, s <- wf
\* A failed, output-free Writer has not earned completion credit. Publishing
\* its repair restores authority to try the Writer, not authority to review.
Init == /\ wf = [w \in {"work"} |-> [W!InitialWork(w) EXCEPT
                    !.phase = "delivered", !.failed = TRUE,
                    !.routeMode = "foreground"]]
        /\ prefix = 0 /\ published = FALSE /\ interrupted = FALSE
        /\ crashUsed = FALSE /\ taskCount = 2 /\ history = TRUE
WritePart == /\ ~interrupted /\ ~published /\ prefix < 5 /\ prefix' = prefix + 1
             /\ UNCHANGED <<wf, published, interrupted, crashUsed, taskCount, history>>
Crash == /\ ~crashUsed /\ prefix > 0 /\ ~published
         /\ crashUsed' = TRUE /\ interrupted' = TRUE
         /\ UNCHANGED <<wf, prefix, published, taskCount, history>>
Retry == /\ interrupted /\ interrupted' = FALSE
         /\ history' = PreserveHistory
         /\ UNCHANGED <<wf, prefix, published, crashUsed, taskCount>>
Publish == /\ ~interrupted /\ ~published /\ (prefix = 5 \/ ~FinishPublication)
           /\ W!Recover("work") /\ published' = TRUE /\ taskCount' = 2
           /\ UNCHANGED <<prefix, interrupted, crashUsed, history>>
Replay == /\ published /\ taskCount' = IF IdempotentReplay THEN taskCount ELSE taskCount + 1
          /\ UNCHANGED <<wf, prefix, published, interrupted, crashUsed, history>>
Next == WritePart \/ Crash \/ Retry \/ Publish \/ Replay
Spec == Init /\ [][Next]_vars /\ WF_vars(WritePart) /\ WF_vars(Retry) /\ WF_vars(Publish)
NoEarlyAuthority == wf["work"].phase = "authorized" => prefix = 5
RetainedHistory == history
OneReservation == taskCount = 2
MappedAuthority == published = (wf["work"].phase = "authorized")
RecoveryProgress == <>published
=============================================================================
