------------------------ MODULE ApprovalConnection ------------------------
EXTENDS Naturals, Sequences
CONSTANTS BindApproval, CheckStop
VARIABLES wf, requestedCandidate
vars == <<wf, requestedCandidate>>
W == INSTANCE Workflow WITH Works <- {"work"}, ReviewOnly <- {},
     MaxFailures <- 0, DeliverInputs <- TRUE, CheckEvidence <- TRUE,
     FenceResume <- TRUE, GuardEffect <- TRUE, TruthfulObservation <- TRUE,
     GuardCleanup <- TRUE, GuardVersion <- TRUE, GuardBaseline <- TRUE,
     GuardLandingBaseline <- TRUE, CanonicalKinds <- {"pass"},
     CorruptPrewrite <- FALSE, TargetKinds <- {"unchanged"},
     GuardReviewRoute <- TRUE, LandingKinds <- {"pass"}, s <- wf
\* Upstream evidence is checked separately. This boundary starts with a
\* verified candidate and an operator's concrete landing request.
Init == /\ requestedCandidate \in {"current", "stale"}
        /\ wf = [w \in {"work"} |-> [W!InitialWork(w) EXCEPT !.phase = "verified"]]
Intent == /\ (requestedCandidate = "current" \/ ~BindApproval)
          /\ W!Intent("work") /\ UNCHANGED requestedCandidate
Cancel == /\ W!Cancel("work") /\ UNCHANGED requestedCandidate
Effect == /\ W!GitEffect("work") /\ UNCHANGED requestedCandidate
\* Removing the final stop check admits a raw effect outside the checked
\* Workflow action. The cancellation invariant must detect that bypass.
UncheckedEffect == /\ ~CheckStop /\ wf["work"].phase = "intent" /\ wf["work"].canceled
                   /\ wf' = [wf EXCEPT !["work"].phase = "git", !["work"].git = TRUE,
                                        !["work"].effects = 1]
                   /\ UNCHANGED requestedCandidate
Next == Intent \/ Cancel \/ Effect \/ UncheckedEffect
Spec == Init /\ [][Next]_vars
CurrentApproval == wf["work"].git => requestedCandidate = "current"
CurrentStop == W!CancellationSafe
=============================================================================
