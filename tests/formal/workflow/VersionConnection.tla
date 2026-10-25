------------------------- MODULE VersionConnection -------------------------
EXTENDS Naturals, Sequences
CONSTANTS CheckVersion, Refused, BeforeDigest, AfterDigest, PendingPublication
VARIABLES wf, completed, retained
vars == <<wf, completed, retained>>
W == INSTANCE Workflow WITH Works <- {"work"}, ReviewOnly <- {},
     MaxFailures <- 0, DeliverInputs <- TRUE, CheckEvidence <- TRUE,
     FenceResume <- TRUE, GuardEffect <- TRUE, TruthfulObservation <- TRUE,
     GuardCleanup <- TRUE, GuardVersion <- CheckVersion, GuardBaseline <- TRUE,
     GuardLandingBaseline <- TRUE, CanonicalKinds <- {"pass"},
     CorruptPrewrite <- FALSE, TargetKinds <- {"unchanged"},
     GuardReviewRoute <- TRUE, LandingKinds <- {"pass"}, s <- wf
Init == /\ wf = [w \in {"work"} |-> [W!InitialWork(w) EXCEPT
                  !.phase = IF PendingPublication THEN "git" ELSE "authorized",
                  !.git = PendingPublication, !.pending = PendingPublication,
                  !.effects = IF PendingPublication THEN 1 ELSE 0]]
        /\ completed = FALSE /\ retained = BeforeDigest
Refuse == /\ ~completed /\ Refused /\ completed' = TRUE
          /\ retained' = AfterDigest /\ UNCHANGED wf
Mutate == /\ ~completed /\ ~Refused /\ W!VersionTransition("work")
          /\ completed' = TRUE /\ retained' = AfterDigest
Next == Refuse \/ Mutate
Spec == Init /\ [][Next]_vars /\ WF_vars(Next)
NoUnsupportedMutation == W!VersionSafe
RetainedState == retained = BeforeDigest
ObservedRefusal == <>completed
=============================================================================
