----------------------- MODULE EvidenceTrace -----------------------
EXTENDS Workflow, Sequences

CONSTANTS EvidenceWork, EvidenceState1, EvidenceState2,
          EvidenceState3, EvidenceState4
EvidenceStates == <<EvidenceState1, EvidenceState2,
                    EvidenceState3, EvidenceState4>>

VARIABLE evidenceIndex
evidenceVars == <<s, evidenceIndex>>

\* The ready Work snapshot establishes this starting boundary. The raw
\* projector independently checks its reviews, Tester, Learner, and identity.
EvidenceInit ==
    /\ s = [w \in Works |-> LET base == InitialWork(w) IN
           [base EXCEPT !.phase = "ready",
                        !.delivered = base.required,
                        !.rawEvidence = {EvidenceId(w, 1)},
                        !.credit = {EvidenceId(w, 1)},
                        !.acceptedCandidate = CandidateId(w),
                        !.learnerMode = "capture",
                        !.expertise = 1]]
    /\ evidenceIndex = 0 /\ EvidenceState1 = "ready"

EvidenceNext ==
    \/ /\ evidenceIndex = 0
       /\ TargetBaseline(EvidenceWork, "unchanged")
       /\ evidenceIndex' = 1 /\ EvidenceState2 = "target-advanced"
    \/ /\ evidenceIndex = 1 /\ s[EvidenceWork].landingOutcome = "unset"
       /\ LandingResult(EvidenceWork, "pass")
       /\ UNCHANGED evidenceIndex
    \/ /\ evidenceIndex = 1 /\ s[EvidenceWork].landingOutcome = "pass"
       /\ s[EvidenceWork].phase = "ready"
       /\ LandingVerify(EvidenceWork)
       /\ UNCHANGED evidenceIndex
    \/ /\ evidenceIndex = 1 /\ s[EvidenceWork].phase = "verified"
       /\ Intent(EvidenceWork) /\ UNCHANGED evidenceIndex
    \/ /\ evidenceIndex = 1 /\ s[EvidenceWork].phase = "intent"
       /\ GitEffect(EvidenceWork) /\ UNCHANGED evidenceIndex
    \/ /\ evidenceIndex = 1 /\ s[EvidenceWork].phase = "git"
       /\ PublishResult(EvidenceWork)
       /\ evidenceIndex' = 2 /\ EvidenceState3 = "landed"
    \/ /\ evidenceIndex = 2 /\ Observe(EvidenceWork)
       /\ evidenceIndex' = 3 /\ EvidenceState4 = "observed"
       /\ s'[EvidenceWork].observed = "published"

EvidenceSpec == EvidenceInit /\ [][EvidenceNext]_evidenceVars
                /\ WF_evidenceVars(EvidenceNext)
EvidenceComplete == <> (evidenceIndex = 3)
====================================================================
