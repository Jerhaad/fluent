------------------------ MODULE EvidenceConnection ------------------------
EXTENDS Naturals, FiniteSets, Sequences
CONSTANTS ScopeCredit, CurrentCandidate
VARIABLES wf, required, attached, reviewed, accepted, proofCandidate
vars == <<wf, required, attached, reviewed, accepted, proofCandidate>>
W == INSTANCE Workflow WITH Works <- {"work"}, ReviewOnly <- {},
     MaxFailures <- 0, DeliverInputs <- TRUE, CheckEvidence <- TRUE,
     FenceResume <- TRUE, GuardEffect <- TRUE, TruthfulObservation <- TRUE,
     GuardCleanup <- TRUE, GuardVersion <- TRUE, GuardBaseline <- TRUE,
     GuardLandingBaseline <- TRUE, CanonicalKinds <- {"pass"},
     CorruptPrewrite <- FALSE, TargetKinds <- {"unchanged"},
     GuardReviewRoute <- TRUE, LandingKinds <- {"pass"}, s <- wf
Init == /\ required \in {{1},{1,2}} /\ attached = {} /\ reviewed = {} /\ accepted = {}
        /\ proofCandidate \in {"current", "stale"}
        /\ wf = [w \in {"work"} |-> LET base == W!InitialWork(w) IN
             [base EXCEPT !.phase = "reviewed", !.delivered = base.required]]
Attach == /\ \E c \in required \ attached:
                 /\ attached' = attached \cup {c}
                 /\ UNCHANGED <<wf, required, reviewed, accepted, proofCandidate>>
Review == /\ \E c \in attached \ reviewed:
                 /\ reviewed' = reviewed \cup {c}
                 /\ accepted' = IF proofCandidate = "current" \/ ~CurrentCandidate
                                  THEN IF ScopeCredit THEN accepted \cup {c} ELSE required
                                  ELSE accepted
                 /\ UNCHANGED <<wf, required, attached, proofCandidate>>
Credit == /\ required \subseteq accepted /\ W!TargetedReview("work")
          /\ UNCHANGED <<required, attached, reviewed, accepted, proofCandidate>>
Result == /\ wf["work"].credit # {} /\ W!CanonicalResult("work", "pass")
          /\ UNCHANGED <<required, attached, reviewed, accepted, proofCandidate>>
Tester == /\ W!Tester("work") /\ UNCHANGED <<required, attached, reviewed, accepted, proofCandidate>>
Next == Attach \/ Review \/ Credit \/ Result \/ Tester
Spec == Init /\ [][Next]_vars
ScopedCredit == accepted \subseteq reviewed
CurrentCredit == accepted # {} => proofCandidate = "current"
TesterSafe == wf["work"].phase = "tested" => required \subseteq accepted
=============================================================================
