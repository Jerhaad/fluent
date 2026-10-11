----------------------- MODULE RequirementConnection -----------------------
EXTENDS Naturals, Sequences
CONSTANT KeepSection
VARIABLES wf, delivered
vars == <<wf, delivered>>
Required == {<<"tester-task", n>> : n \in 20..33} \cup {<<"public-workflow-continuation", 20>>}
W == INSTANCE Workflow WITH Works <- {"work"}, ReviewOnly <- {},
     MaxFailures <- 0, DeliverInputs <- TRUE, CheckEvidence <- TRUE,
     FenceResume <- TRUE, GuardEffect <- TRUE, TruthfulObservation <- TRUE,
     GuardCleanup <- TRUE, GuardVersion <- TRUE, GuardBaseline <- TRUE,
     GuardLandingBaseline <- TRUE, CanonicalKinds <- {"pass"},
     CorruptPrewrite <- FALSE, TargetKinds <- {"unchanged"},
     GuardReviewRoute <- TRUE, LandingKinds <- {"pass"}, s <- wf
Init == /\ wf = [w \in {"work"} |-> [W!InitialWork(w) EXCEPT
               !.phase = "dispatched", !.required = Required,
               !.serviceOwner = W!ServiceId(1), !.routeMode = "foreground"]]
        /\ delivered = {}
Deliver == /\ W!Deliver("work")
           /\ delivered' = IF KeepSection THEN Required ELSE {<<"unqualified", n>> : n \in 20..33}
Next == Deliver
Spec == Init /\ [][Next]_vars
MappedInput == wf["work"].phase = "delivered" => delivered = wf["work"].delivered
AllIdentities == wf["work"].phase = "delivered" => Required \subseteq delivered
=============================================================================
