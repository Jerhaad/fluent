------------------------ MODULE LearningConnection ------------------------
EXTENDS Naturals, Sequences
CONSTANTS PreserveMode, RequireConfinement, PreserveCandidate
VARIABLES wf, mode, outcome, runs, candidateChanged, handoff, confined
vars == <<wf, mode, outcome, runs, candidateChanged, handoff, confined>>
W == INSTANCE Workflow WITH Works <- {"work"}, ReviewOnly <- {},
     MaxFailures <- 0, DeliverInputs <- TRUE, CheckEvidence <- TRUE,
     FenceResume <- TRUE, GuardEffect <- TRUE, TruthfulObservation <- TRUE,
     GuardCleanup <- TRUE, GuardVersion <- TRUE, GuardBaseline <- TRUE,
     GuardLandingBaseline <- TRUE, CanonicalKinds <- {"pass"},
     CorruptPrewrite <- FALSE, TargetKinds <- {"unchanged"},
     GuardReviewRoute <- TRUE, LandingKinds <- {"pass"}, s <- wf
Init == /\ mode \in {"capture", "no-expertise"}
        /\ confined \in BOOLEAN /\ outcome = "failed" /\ runs = 1
        /\ candidateChanged = FALSE /\ handoff = FALSE
        /\ wf = [w \in {"work"} |-> LET base == W!InitialWork(w) IN
             [base EXCEPT !.phase = "tested", !.delivered = base.required,
                          !.rawEvidence = {W!EvidenceId(w,1)},
                          !.credit = {W!EvidenceId(w,1)},
                          !.acceptedCandidate = W!CandidateId(w)]]
Retry == /\ outcome = "failed" /\ runs = 1
         /\ runs' = 2 /\ outcome' = "running"
         /\ mode' = IF PreserveMode THEN mode ELSE "capture"
         /\ UNCHANGED <<wf, candidateChanged, handoff, confined>>
Refuse == /\ outcome = "running" /\ mode = "no-expertise" /\ ~confined
          /\ RequireConfinement /\ outcome' = "failed"
          /\ UNCHANGED <<wf, mode, runs, candidateChanged, handoff, confined>>
Capture == /\ outcome = "running" /\ mode = "capture"
           /\ W!LearnCapture("work") /\ outcome' = "succeeded"
           /\ candidateChanged' = TRUE /\ handoff' = TRUE
           /\ UNCHANGED <<mode, runs, confined>>
NoExpertise == /\ outcome = "running" /\ mode = "no-expertise"
               /\ (confined \/ ~RequireConfinement)
               /\ W!LearnNoExpertise("work") /\ outcome' = "succeeded"
               /\ candidateChanged' = ~PreserveCandidate /\ handoff' = TRUE
               /\ UNCHANGED <<mode, runs, confined>>
Ready == /\ W!Ready("work") /\ UNCHANGED <<mode, outcome, runs, candidateChanged, handoff, confined>>
Next == Retry \/ Refuse \/ Capture \/ NoExpertise \/ Ready
Spec == Init /\ [][Next]_vars
CandidateSafe == mode = "no-expertise" => ~candidateChanged
ConfinementSafe == mode = "no-expertise" /\ outcome = "succeeded" => confined
HandoffSafe == (wf["work"].phase \in {"learned", "ready"}) = handoff
\* The unchanged selected mode is checked as a temporal property because its
\* initial value is nondeterministic, independently of runtime availability.
ModeStable == [][mode' = mode]_vars
=============================================================================
