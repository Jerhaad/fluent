--------------------------- MODULE Workflow ---------------------------
EXTENDS Naturals, FiniteSets, TLC

CONSTANTS Works, ReviewOnly, MaxFailures, DeliverInputs, CheckEvidence,
          FenceResume, GuardEffect, TruthfulObservation, GuardCleanup,
          GuardVersion
ASSUME /\ Works # {} /\ ReviewOnly \subseteq Works
       /\ MaxFailures \in Nat
       /\ DeliverInputs \in BOOLEAN /\ CheckEvidence \in BOOLEAN
       /\ FenceResume \in BOOLEAN /\ GuardEffect \in BOOLEAN
       /\ TruthfulObservation \in BOOLEAN /\ GuardCleanup \in BOOLEAN
       /\ GuardVersion \in BOOLEAN

\* Typed tuples keep protocol identities distinct even when two Work Items
\* have the same local ordinal. They are abstract names, not persisted paths.
AttemptId(w) == <<"attempt", w>>
TaskId(w) == <<"task", w>>
WorkspaceId(w) == <<"workspace", w>>
CandidateId(w) == <<"candidate", w>>
DispatchId(w) == <<"dispatch", w>>
ServiceId(g) == <<"service", g>>
WorkerId(w, g) == <<"worker", w, g>>
RecoveryId(w, g) == <<"recovery", w, g>>

VARIABLE s
vars == <<s>>
Init == s = [w \in Works |->
    [phase |-> "new", required |-> TRUE, delivered |-> FALSE,
     rawEvidence |-> FALSE, credit |-> FALSE,
     canceled |-> FALSE, authority |-> 0, generation |-> 1,
     failures |-> 0, failed |-> FALSE, ioFailed |-> FALSE,
     effects |-> 0, observed |-> "none", cleaned |-> FALSE,
     pending |-> FALSE, published |-> FALSE, git |-> FALSE,
     badResume |-> FALSE, badVersion |-> FALSE]]

Intake(w) == /\ s[w].phase = "new" /\ s' = [s EXCEPT ![w].phase = "intake"]
Authorize(w) == /\ s[w].phase = "intake" /\ ~s[w].failed
                /\ s' = [s EXCEPT ![w].phase = "authorized"]
Dispatch(w) == /\ w \notin ReviewOnly /\ s[w].phase = "authorized"
               /\ ~s[w].failed /\ ~s[w].canceled
               /\ s' = [s EXCEPT ![w].phase = "dispatched"]
Deliver(w) == /\ s[w].phase = "dispatched" /\ ~s[w].failed
              /\ ~s[w].canceled
              /\ s' = [s EXCEPT ![w].phase = "delivered",
                                    ![w].delivered = DeliverInputs]
Writer(w) == /\ s[w].phase = "delivered" /\ ~s[w].failed
             /\ ~s[w].canceled
             /\ s' = [s EXCEPT ![w].phase = "written"]
Review(w) == /\ s[w].phase = "written" /\ ~s[w].failed
             /\ ~s[w].canceled
             /\ s' = [s EXCEPT ![w].phase = "reviewed",
                                   ![w].rawEvidence = TRUE,
                                   ![w].credit = CheckEvidence]
TargetedReview(w) == /\ s[w].phase = "reviewed" /\ ~s[w].credit
                     /\ CheckEvidence /\ ~s[w].failed
                     /\ s' = [s EXCEPT ![w].credit = TRUE]
Tester(w) == /\ s[w].phase = "reviewed" /\ ~s[w].failed
             /\ ~s[w].canceled
             /\ s' = [s EXCEPT ![w].phase = "tested"]
LearnCapture(w) == /\ s[w].phase = "tested" /\ ~s[w].failed
                   /\ ~s[w].canceled
                   /\ s' = [s EXCEPT ![w].phase = "learned"]
LearnNoExpertise(w) == LearnCapture(w)
Ready(w) == /\ s[w].phase = "learned" /\ ~s[w].failed
            /\ ~s[w].canceled
            /\ s' = [s EXCEPT ![w].phase = "ready"]
LandingVerify(w) == /\ s[w].phase = "ready" /\ ~s[w].failed
                    /\ ~s[w].canceled
                    /\ s' = [s EXCEPT ![w].phase = "verified"]
ReviewOnlyStart(w) == /\ w \in ReviewOnly
                      /\ s[w].phase = "authorized" /\ ~s[w].failed
                      /\ ~s[w].canceled
                      /\ s' = [s EXCEPT ![w].phase = "written",
                                            ![w].delivered = TRUE]
Intent(w) == /\ s[w].phase = "verified" /\ ~s[w].failed
             /\ ~s[w].canceled
             /\ s' = [s EXCEPT ![w].phase = "intent",
                                   ![w].pending = TRUE]
GitEffect(w) == /\ s[w].phase = "intent" /\ ~s[w].failed
                /\ ~s[w].canceled /\ ~s[w].ioFailed
                /\ s' = [s EXCEPT ![w].phase = "git",
                                      ![w].git = TRUE,
                                      ![w].effects = @ + 1]
PublishResult(w) == /\ s[w].phase = "git" /\ ~s[w].ioFailed
                    /\ ~s[w].failed
                    /\ s' = [s EXCEPT ![w].phase = "published",
                                          ![w].published = TRUE,
                                          ![w].pending = FALSE]
FollowUpStart(w) == /\ s[w].phase = "published" /\ ~s[w].failed
                    /\ s' = [s EXCEPT ![w].phase = "follow-up-pending"]
FollowUp(w) == /\ s[w].phase = "follow-up-pending" /\ ~s[w].failed
               /\ s' = [s EXCEPT ![w].phase = "follow-up"]
RetryEffect(w) == /\ s[w].phase = "git" /\ ~s[w].failed
                  /\ s' = [s EXCEPT ![w].effects = IF GuardEffect THEN @ ELSE @ + 1]
Observe(w) == /\ s[w].observed = "none"
              /\ s' = [s EXCEPT ![w].observed =
                          IF TruthfulObservation THEN s[w].phase ELSE "bad"]
Cleanup(w) == /\ ~s[w].cleaned
              /\ (s[w].phase = "follow-up" \/ ~GuardCleanup)
              /\ s' = [s EXCEPT ![w].cleaned = TRUE]
Cancel(w) == /\ ~s[w].canceled /\ ~s[w].git
             /\ s[w].phase \notin {"new", "published", "follow-up"}
             /\ s' = [s EXCEPT ![w].canceled = TRUE,
                                   ![w].authority = s[w].generation]
ReplaceGeneration(w) == /\ s[w].canceled /\ s[w].generation = 1
                        /\ s' = [s EXCEPT ![w].generation = 2]
Resume(w) == /\ s[w].canceled
             /\ (s[w].authority = s[w].generation \/ ~FenceResume)
             /\ s' = [s EXCEPT ![w].canceled = FALSE,
                                   ![w].badResume = s[w].authority # s[w].generation]
FaultBudgetAvailable(w) ==
    /\ s[w].failures < MaxFailures
    /\ (MaxFailures # 1 \/ \A x \in Works: s[x].failures = 0)
Crash(w) == /\ ~s[w].failed /\ FaultBudgetAvailable(w)
            /\ s[w].phase \notin {"new", "follow-up"}
            /\ s' = [s EXCEPT ![w].failed = TRUE,
                                  ![w].failures = @ + 1]
Recover(w) == /\ s[w].failed /\ s' = [s EXCEPT ![w].failed = FALSE]
FailWrite(w) == /\ ~s[w].ioFailed /\ FaultBudgetAvailable(w)
                /\ s[w].phase \in {"intent", "git", "follow-up-pending"}
                /\ s' = [s EXCEPT ![w].ioFailed = TRUE,
                                      ![w].failures = @ + 1]
RecoverIO(w) == /\ s[w].ioFailed /\ s' = [s EXCEPT ![w].ioFailed = FALSE]
VersionTransition(w) == /\ ~s[w].badVersion
                        /\ (s[w].phase \in {"new", "follow-up"} \/ ~GuardVersion)
                        /\ s' = [s EXCEPT ![w].badVersion =
                                      s[w].phase \notin {"new", "follow-up"}]

Next == \E w \in Works:
    Intake(w) \/ Authorize(w) \/ Dispatch(w) \/ Deliver(w) \/ Writer(w)
    \/ Review(w) \/ TargetedReview(w) \/ Tester(w) \/ LearnCapture(w)
    \/ LearnNoExpertise(w) \/ Ready(w) \/ LandingVerify(w)
    \/ ReviewOnlyStart(w) \/ Intent(w) \/ GitEffect(w) \/ PublishResult(w)
    \/ FollowUpStart(w) \/ FollowUp(w) \/ RetryEffect(w)
    \/ Observe(w) \/ Cleanup(w) \/ Cancel(w) \/ ReplaceGeneration(w)
    \/ Resume(w) \/ Crash(w) \/ Recover(w) \/ FailWrite(w)
    \/ RecoverIO(w) \/ VersionTransition(w)
Spec == Init /\ [][Next]_vars
FairSpec == Spec /\ (\A w \in Works: WF_vars(Recover(w))
                                   /\ WF_vars(RecoverIO(w))
                                   /\ WF_vars(PublishResult(w)))

InputSafe == \A w \in Works:
    s[w].phase \notin {"written", "reviewed", "tested", "learned", "ready",
                       "verified", "intent", "git", "published", "follow-up-pending",
                       "follow-up"} \/ s[w].delivered
EvidenceSafe == \A w \in Works:
    s[w].phase \notin {"tested", "learned", "ready", "verified", "intent", "git",
                       "published", "follow-up-pending", "follow-up"} \/ s[w].credit
ResumeSafe == \A w \in Works: ~s[w].badResume
OneEffect == \A w \in Works: s[w].effects <= 1
ObservationSafe == \A w \in Works:
    s[w].observed # "bad"
CleanupSafe == \A w \in Works:
    ~s[w].cleaned \/ s[w].phase = "follow-up"
VersionSafe == \A w \in Works: ~s[w].badVersion
PublicationSafe == \A w \in Works:
    (s[w].published => s[w].git) /\ (s[w].git => s[w].pending \/ s[w].published)
CancellationSafe == \A w \in Works: s[w].canceled => ~s[w].git

\* Progress depends on a finite failure budget, enabled recovery, and a
\* continuing public scheduler/operator. FairSpec states only local recovery
\* and result-publication fairness; the later integrated check owns end to end
\* progress and human-decision fairness.
RecoveryProgress == \A w \in Works: s[w].failed ~> ~s[w].failed
=============================================================================
