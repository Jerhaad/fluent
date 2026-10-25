---------------------- MODULE AuthorizationConnection ----------------------
EXTENDS Naturals, Sequences
CONSTANT CheckAuthority
VARIABLES wf, authority, admitted
vars == <<wf, authority, admitted>>
W == INSTANCE Workflow WITH Works <- {"work"}, ReviewOnly <- {},
     MaxFailures <- 0, DeliverInputs <- TRUE, CheckEvidence <- TRUE,
     FenceResume <- TRUE, GuardEffect <- TRUE, TruthfulObservation <- TRUE,
     GuardCleanup <- TRUE, GuardVersion <- TRUE, GuardBaseline <- TRUE,
     GuardLandingBaseline <- TRUE, CanonicalKinds <- {"pass"},
     CorruptPrewrite <- FALSE, TargetKinds <- {"unchanged"},
     GuardReviewRoute <- TRUE, LandingKinds <- {"pass"}, s <- wf
Init == /\ W!Init /\ authority \in {"human-current", "absent"} /\ admitted = FALSE
Intake == /\ W!Intake("work") /\ UNCHANGED <<authority, admitted>>
Authorize == /\ (authority = "human-current" \/ ~CheckAuthority)
             /\ W!Authorize("work") /\ UNCHANGED <<authority, admitted>>
Enqueue == /\ W!Enqueue("work") /\ admitted' = TRUE /\ UNCHANGED authority
Repeat == /\ admitted /\ UNCHANGED vars
Next == Intake \/ Authorize \/ Enqueue \/ Repeat
Spec == Init /\ [][Next]_vars
AuthorizedAdmission == admitted => authority = "human-current"
MappedAdmission == admitted = wf["work"].queued
=============================================================================
