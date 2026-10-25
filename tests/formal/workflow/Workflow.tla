--------------------------- MODULE Workflow ---------------------------
EXTENDS Naturals, FiniteSets, TLC

CONSTANTS Works, ReviewOnly, MaxFailures, DeliverInputs, CheckEvidence,
          FenceResume, GuardEffect, TruthfulObservation, GuardCleanup,
          GuardVersion, GuardBaseline, GuardLandingBaseline, CanonicalKinds,
          CorruptPrewrite, TargetKinds, GuardReviewRoute, LandingKinds
ASSUME /\ Works # {} /\ ReviewOnly \subseteq Works
       /\ MaxFailures \in Nat
       /\ DeliverInputs \in BOOLEAN /\ CheckEvidence \in BOOLEAN
       /\ FenceResume \in BOOLEAN /\ GuardEffect \in BOOLEAN
       /\ TruthfulObservation \in BOOLEAN /\ GuardCleanup \in BOOLEAN
       /\ GuardVersion \in BOOLEAN
       /\ GuardBaseline \in BOOLEAN /\ GuardLandingBaseline \in BOOLEAN
       /\ CanonicalKinds \subseteq {"pass", "known", "new", "incomplete",
                                     "harness", "leak", "unexplained",
                                     "misbound", "reference-fail"}
       /\ CorruptPrewrite \in BOOLEAN
       /\ GuardReviewRoute \in BOOLEAN
       /\ TargetKinds \subseteq {"unchanged", "cleared", "untrusted", "misbound"}
       /\ LandingKinds \subseteq {"pass", "known", "new", "incomplete",
                                   "harness", "leak", "unexplained",
                                   "misbound", "reference-fail"}

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
RequirementId(w) == <<"requirement", w, 1>>
ContextId(w) == <<"context", w, 1>>
ReviewScope(w) == <<"scope", w, CandidateId(w)>>
EvidenceId(w, g) == <<"evidence", w, g, CandidateId(w),
                       RequirementId(w), ContextId(w), ReviewScope(w)>>
StaleCandidateEvidenceId(w, g) == <<"evidence", w, g, <<"candidate", "old">>,
                                     RequirementId(w), ContextId(w), ReviewScope(w)>>
WrongRequirementEvidenceId(w, g) == <<"evidence", w, g, CandidateId(w),
                                       <<"requirement", "other">>, ContextId(w),
                                       ReviewScope(w)>>
WrongContextEvidenceId(w, g) == <<"evidence", w, g, CandidateId(w),
                                   RequirementId(w), <<"context", "old">>,
                                   ReviewScope(w)>>
WrongScopeEvidenceId(w, g) == <<"evidence", w, g, CandidateId(w),
                                 RequirementId(w), ContextId(w),
                                 <<"scope", "other">>>>
Unowned == <<"service", 0>>

VARIABLE s
vars == <<s>>
CurrentEvidence(w, g) == <<"evidence", w, g, s[w].candidate,
                            CHOOSE r \in s[w].required: TRUE,
                            s[w].context, s[w].reviewScope>>
InitialWork(w) ==
    [phase |-> "new", route |-> IF w \in ReviewOnly THEN "review-only" ELSE "ordinary",
     attempt |-> AttemptId(w), task |-> TaskId(w),
     workspace |-> WorkspaceId(w), candidate |-> IF w \in ReviewOnly
         THEN "none" ELSE CandidateId(w),
     dispatch |-> DispatchId(w), worker |-> WorkerId(w, 1),
     recovery |-> RecoveryId(w, 1),
     required |-> {RequirementId(w)}, delivered |-> {},
     context |-> ContextId(w), reviewScope |-> IF w \in ReviewOnly
         THEN <<"scope", w, "review-only">> ELSE ReviewScope(w),
     rawEvidence |-> {}, credit |-> {}, reviewKind |-> "none",
     canceled |-> FALSE, authority |-> 0, generation |-> 1,
     serviceOwner |-> Unowned, version |-> 1,
     failures |-> 0, failed |-> FALSE, ioFailed |-> FALSE,
     failedDuringOwnership |-> FALSE,
     recoveredBeforeEffect |-> FALSE,
     effects |-> 0, observed |-> "none", cleaned |-> FALSE,
     observedFailedWrite |-> FALSE,
     pending |-> FALSE, published |-> FALSE, git |-> FALSE,
     badResume |-> FALSE, badVersion |-> FALSE,
     routeMode |-> "unset", queued |-> FALSE,
     correction |-> FALSE, correctedAfterRecovery |-> FALSE,
     rechecked |-> FALSE,
     acceptedCandidate |-> "none",
     prewriteCommit |-> <<"source", w>>,
     prewriteTrusted |-> TRUE,
     prewriteFailures |-> {<<"test", "known">>},
     targetCommit |-> <<"target", w, 1>>,
     targetBaselineCommit |-> <<"target", w, 1>>,
     targetFailures |-> {<<"test", "known">>},
     targetTrusted |-> TRUE,
     canonicalOutcome |-> "unset", canonicalFailures |-> {},
     canonicalComplete |-> FALSE, canonicalHarnessError |-> FALSE,
     canonicalProcessLeak |-> FALSE, canonicalUnexplained |-> FALSE,
     canonicalCandidate |-> "none", canonicalReferencePassed |-> FALSE,
     testedFailures |-> {}, testedComplete |-> FALSE,
     testedReferencePassed |-> FALSE,
     landingOutcome |-> "unset", landingCandidate |-> "none",
     landingFailures |-> {}, landingComplete |-> FALSE,
     landingHarnessError |-> FALSE, landingProcessLeak |-> FALSE,
     landingUnexplained |-> FALSE, landingReferencePassed |-> FALSE,
     learnerMode |-> "unset", expertise |-> 0,
     expertiseBeforeLearning |-> 0]
Init == s = [w \in Works |-> InitialWork(w)]

Intake(w) == /\ s[w].phase = "new" /\ s' = [s EXCEPT ![w].phase = "intake"]
AttemptCreate(w) == /\ s[w].phase = "intake" /\ ~s[w].canceled
                    /\ s[w].attempt = AttemptId(w)
                    /\ s' = [s EXCEPT ![w].phase = "planned"]
Authorize(w) == /\ s[w].phase \in {"intake", "planned"} /\ ~s[w].failed
                /\ ~s[w].canceled
                /\ s' = [s EXCEPT ![w].phase = "authorized"]
ChooseForeground(w) == /\ w \notin ReviewOnly /\ s[w].phase = "authorized"
                       /\ s[w].routeMode = "unset"
                       /\ s' = [s EXCEPT ![w].routeMode = "foreground"]
Enqueue(w) == /\ w \notin ReviewOnly /\ s[w].phase = "authorized"
              /\ s[w].routeMode = "unset"
              /\ s' = [s EXCEPT ![w].routeMode = "queued", ![w].queued = TRUE]
Dispatch(w) == /\ w \notin ReviewOnly /\ s[w].phase = "authorized"
               /\ ~s[w].failed /\ ~s[w].canceled
               /\ s[w].routeMode \in {"foreground", "queued"}
               /\ (s[w].routeMode = "queued" => s[w].queued)
               /\ \A x \in Works \ {w}: s[x].serviceOwner = Unowned
               /\ s[w].attempt = AttemptId(w) /\ s[w].task = TaskId(w)
               /\ s[w].workspace = WorkspaceId(w)
               /\ s[w].dispatch = DispatchId(w)
               /\ s' = [s EXCEPT ![w].phase = "dispatched", ![w].queued = FALSE,
                                     ![w].serviceOwner = ServiceId(s[w].generation)]
Deliver(w) == /\ s[w].phase = "dispatched" /\ ~s[w].failed
              /\ ~s[w].canceled
              /\ s' = [s EXCEPT ![w].phase = "delivered",
                                    ![w].delivered = IF DeliverInputs THEN s[w].required ELSE {}]
Writer(w) == /\ s[w].phase = "delivered" /\ ~s[w].failed
             /\ ~s[w].canceled
             /\ s[w].worker = WorkerId(w, s[w].generation)
             /\ s' = [s EXCEPT ![w].phase = "written",
                                   ![w].serviceOwner = Unowned]
Review(w) == /\ s[w].phase = "written" /\ ~s[w].failed
             /\ ~s[w].canceled
             /\ s' = [s EXCEPT ![w].phase = "reviewed",
                                   ![w].reviewKind = "fresh",
                                   ![w].rawEvidence = @ \cup {CurrentEvidence(w, s[w].generation)},
                                   ![w].credit = IF CheckEvidence
                                       THEN {CurrentEvidence(w, s[w].generation)}
                                       ELSE {StaleCandidateEvidenceId(w, s[w].generation)}]
StaleEvidence(w) == /\ s[w].phase = "written" /\ ~s[w].failed
                   /\ s' = [s EXCEPT ![w].phase = "reviewed",
                                        ![w].rawEvidence = {EvidenceId(w, 0)},
                                        ![w].credit = {}, ![w].reviewKind = "stale"]
UnrelatedEvidence(w) == /\ s[w].phase = "written" /\ ~s[w].failed
                       /\ s' = [s EXCEPT ![w].phase = "reviewed",
                                            ![w].rawEvidence = {<<"evidence", "other">>},
                                            ![w].credit = {}, ![w].reviewKind = "unrelated"]
MisboundEvidence(w, kind) ==
    /\ s[w].phase = "written" /\ ~s[w].failed
    /\ kind \in {"candidate", "requirement", "context", "scope"}
    /\ s' = [s EXCEPT ![w].phase = "reviewed",
                         ![w].credit = {}, ![w].reviewKind = "misbound",
                         ![w].rawEvidence = {CASE kind = "candidate"
                            -> StaleCandidateEvidenceId(w, s[w].generation)
                          [] kind = "requirement"
                            -> WrongRequirementEvidenceId(w, s[w].generation)
                          [] kind = "context"
                            -> WrongContextEvidenceId(w, s[w].generation)
                          [] OTHER -> WrongScopeEvidenceId(w, s[w].generation)}]
TargetedReview(w) == /\ w \notin ReviewOnly
                     /\ s[w].phase = "reviewed" /\ s[w].credit = {}
                     /\ CheckEvidence /\ ~s[w].failed /\ ~s[w].canceled
                     /\ s' = [s EXCEPT ![w].reviewKind = "targeted",
                                           ![w].rawEvidence = @ \cup {CurrentEvidence(w, s[w].generation)},
                                           ![w].credit = {CurrentEvidence(w, s[w].generation)}]
RecheckSameCandidate(w) == /\ w \notin ReviewOnly
                           /\ s[w].phase = "reviewed" /\ ~s[w].rechecked
                           /\ s[w].credit = {CurrentEvidence(w, s[w].generation)}
                           /\ s' = [s EXCEPT ![w].phase = "written",
                                                 ![w].rechecked = TRUE]
CarryForward(w) == /\ w \notin ReviewOnly /\ s[w].phase = "written"
                   /\ CurrentEvidence(w, s[w].generation) \in s[w].rawEvidence
                   /\ s' = [s EXCEPT ![w].phase = "reviewed",
                                         ![w].reviewKind = "carried",
                                         ![w].credit = {CurrentEvidence(w, s[w].generation)}]
CorrectCandidate(w) == /\ s[w].phase \in {"reviewed", "tested"}
                       /\ ~s[w].correction /\ ~s[w].failed /\ ~s[w].canceled
                       /\ w \notin ReviewOnly
                       /\ s' = [s EXCEPT ![w].phase = "authorized",
                                             ![w].candidate = <<"candidate", w, 2>>,
                                             ![w].required = {<<"requirement", w, 2>>},
                                             ![w].delivered = {},
                                             ![w].context = <<"context", w, 2>>,
                                             ![w].reviewScope = <<"scope", w, <<"candidate", w, 2>>>>,
                                             ![w].correction = TRUE,
                                             ![w].correctedAfterRecovery =
                                                 s[w].failedDuringOwnership
                                                 /\ s[w].recoveredBeforeEffect,
                                             ![w].credit = {},
                                             ![w].reviewKind = "none",
                                             ![w].routeMode = "unset",
                                             ![w].canonicalOutcome = "unset",
                                             ![w].testedComplete = FALSE,
                                             ![w].learnerMode = "unset"]
\* A complete canonical command can still contain known assertion failures.
\* Reference evidence must independently pass. All cases retain exact test IDs.
CanonicalResult(w, kind) ==
    /\ w \notin ReviewOnly /\ s[w].phase = "reviewed"
    /\ s[w].canonicalOutcome = "unset"
    /\ kind \in {"pass", "known", "new", "incomplete", "harness",
                 "leak", "unexplained", "misbound", "reference-fail"}
    /\ kind \in CanonicalKinds
    /\ s' = [s EXCEPT ![w].canonicalOutcome = kind,
                         ![w].canonicalFailures =
                             IF kind = "known" THEN {<<"test", "known">>}
                             ELSE IF kind = "new" THEN {<<"test", "new">>}
                             ELSE {},
                         ![w].canonicalComplete = kind # "incomplete",
                         ![w].canonicalHarnessError = kind = "harness",
                         ![w].canonicalProcessLeak = kind = "leak",
                         ![w].canonicalUnexplained = kind = "unexplained",
                         ![w].canonicalCandidate = IF kind = "misbound"
                             THEN <<"candidate", "other">> ELSE s[w].candidate,
                         ![w].canonicalReferencePassed = kind # "reference-fail"]
CanonicalSafe(w) ==
    /\ s[w].canonicalOutcome # "unset"
    /\ s[w].canonicalComplete /\ ~s[w].canonicalHarnessError
    /\ ~s[w].canonicalProcessLeak /\ ~s[w].canonicalUnexplained
    /\ s[w].canonicalCandidate = s[w].candidate
    /\ s[w].canonicalReferencePassed
TesterBaselineSafe(w) ==
    /\ CanonicalSafe(w)
    /\ s[w].canonicalFailures \subseteq s[w].prewriteFailures
    /\ s[w].prewriteTrusted
    /\ s[w].prewriteCommit = <<"source", w>>
PrewriteBaselineCorrupt(w, kind) ==
    /\ w \notin ReviewOnly /\ s[w].phase = "reviewed"
    /\ CorruptPrewrite
    /\ kind \in {"untrusted", "misbound"}
    /\ s[w].prewriteTrusted /\ s[w].prewriteCommit = <<"source", w>>
    /\ s' = [s EXCEPT ![w].prewriteTrusted = kind # "untrusted",
                         ![w].prewriteCommit = IF kind = "misbound"
                             THEN <<"source", "other">> ELSE @]
Tester(w) == /\ s[w].phase = "reviewed" /\ ~s[w].failed
             /\ ~s[w].canceled
             /\ w \notin ReviewOnly
             /\ s[w].canonicalOutcome # "unset"
             /\ (CheckEvidence => s[w].credit = {CurrentEvidence(w, s[w].generation)})
             /\ (GuardBaseline => TesterBaselineSafe(w))
             /\ s' = [s EXCEPT ![w].phase = "tested",
                                   ![w].acceptedCandidate = s[w].candidate,
                                   ![w].testedFailures = s[w].canonicalFailures,
                                   ![w].testedComplete = CanonicalSafe(w),
                                   ![w].testedReferencePassed = s[w].canonicalReferencePassed,
                                   ![w].expertiseBeforeLearning = s[w].expertise]
LearnCapture(w) == /\ s[w].phase = "tested" /\ ~s[w].failed
                   /\ ~s[w].canceled
                   /\ s' = [s EXCEPT ![w].phase = "learned",
                                         ![w].learnerMode = "capture",
                                         ![w].expertise = @ + 1]
LearnNoExpertise(w) == /\ s[w].phase = "tested" /\ ~s[w].failed
                       /\ ~s[w].canceled
                       /\ s' = [s EXCEPT ![w].phase = "learned",
                                             ![w].learnerMode = "no-expertise"]
Ready(w) == /\ s[w].phase = "learned" /\ ~s[w].failed
            /\ ~s[w].canceled
            /\ s[w].acceptedCandidate = s[w].candidate
            /\ s' = [s EXCEPT ![w].phase = "ready"]
\* Landing uses the target captured at landing start, not the retained
\* pre-write baseline. A rebase can invalidate an earlier acceptance.
LandingResult(w, kind) ==
    /\ w \notin ReviewOnly /\ s[w].phase = "ready"
    /\ s[w].landingOutcome = "unset" /\ kind \in LandingKinds
    /\ s' = [s EXCEPT ![w].landingOutcome = kind,
                         ![w].landingCandidate = IF kind = "misbound"
                             THEN <<"candidate", "other">> ELSE s[w].candidate,
                         ![w].landingFailures =
                             IF kind = "known" THEN {<<"test", "known">>}
                             ELSE IF kind = "new" THEN {<<"test", "new">>}
                             ELSE {},
                         ![w].landingComplete = kind # "incomplete",
                         ![w].landingHarnessError = kind = "harness",
                         ![w].landingProcessLeak = kind = "leak",
                         ![w].landingUnexplained = kind = "unexplained",
                         ![w].landingReferencePassed = kind # "reference-fail"]
LandingBaselineSafe(w) ==
    /\ s[w].targetTrusted
    /\ s[w].targetBaselineCommit = s[w].targetCommit
    /\ s[w].landingOutcome # "unset"
    /\ s[w].landingCandidate = s[w].candidate
    /\ s[w].landingComplete /\ ~s[w].landingHarnessError
    /\ ~s[w].landingProcessLeak /\ ~s[w].landingUnexplained
    /\ s[w].landingReferencePassed
    /\ s[w].landingFailures \subseteq s[w].targetFailures
LandingVerify(w) == /\ (w \notin ReviewOnly \/ ~GuardReviewRoute)
                    /\ s[w].phase = "ready" /\ ~s[w].failed
                    /\ ~s[w].canceled
                    /\ (s[w].landingOutcome # "unset"
                        \/ (w \in ReviewOnly /\ ~GuardReviewRoute))
                    /\ (GuardLandingBaseline =>
                        (w \in ReviewOnly \/ LandingBaselineSafe(w)))
                    /\ s' = [s EXCEPT ![w].phase = "verified"]
TargetBaseline(w, kind) ==
    /\ w \notin ReviewOnly /\ s[w].phase = "ready"
    /\ kind \in {"unchanged", "cleared", "untrusted", "misbound"}
    /\ kind \in TargetKinds
    /\ s[w].targetCommit = <<"target", w, 1>>
    /\ s' = [s EXCEPT ![w].targetCommit = <<"target", w, 2>>,
                         ![w].targetBaselineCommit = IF kind = "misbound"
                             THEN <<"target", w, 1>> ELSE <<"target", w, 2>>,
                         ![w].targetFailures = IF kind = "cleared" THEN {} ELSE @,
                         ![w].targetTrusted = kind # "untrusted"]
ReviewOnlyStart(w) == /\ w \in ReviewOnly
                      /\ s[w].phase = "authorized" /\ ~s[w].failed
                      /\ ~s[w].canceled
                      /\ s' = [s EXCEPT ![w].phase = "review-only-tested",
                                            ![w].delivered = s[w].required,
                                            ![w].acceptedCandidate = s[w].candidate]
ReviewOnlyReview(w) == /\ w \in ReviewOnly
                       /\ (s[w].phase = "review-only-tested"
                           \/ (s[w].phase = "reviewed" /\ s[w].credit = {}))
                       /\ ~s[w].failed /\ ~s[w].canceled
                       /\ s' = [s EXCEPT ![w].phase = "reviewed",
                                             ![w].reviewKind = "review-only",
                                             ![w].rawEvidence = {CurrentEvidence(w, s[w].generation)},
                                             ![w].credit = {CurrentEvidence(w, s[w].generation)}]
ReviewOnlyReady(w) == /\ w \in ReviewOnly /\ s[w].phase = "reviewed"
                      /\ ~s[w].failed /\ ~s[w].canceled
                      /\ s[w].credit = {CurrentEvidence(w, s[w].generation)}
                      /\ s[w].acceptedCandidate = s[w].candidate
                      /\ s' = [s EXCEPT ![w].phase = IF GuardReviewRoute
                          THEN "review-only-complete" ELSE "ready"]
Intent(w) == /\ (w \notin ReviewOnly \/ ~GuardReviewRoute)
             /\ s[w].phase = "verified" /\ ~s[w].failed
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
                          IF TruthfulObservation THEN s[w].phase ELSE "bad",
                                    ![w].observedFailedWrite =
                          s[w].pending /\ s[w].ioFailed]
Cleanup(w) == /\ ~s[w].cleaned
              /\ (s[w].phase \in {"follow-up", "review-only-complete"} \/ ~GuardCleanup)
              /\ s' = [s EXCEPT ![w].cleaned = TRUE]
Cancel(w) == /\ ~s[w].canceled /\ ~s[w].git
             /\ s[w].phase \notin {"new", "published", "follow-up",
                                    "review-only-complete"}
             /\ s' = [s EXCEPT ![w].canceled = TRUE,
                                   ![w].authority = s[w].generation]
ReplaceGeneration(w) == /\ s[w].canceled /\ s[w].generation = 1
                        /\ s' = [s EXCEPT ![w].generation = 2,
                                              ![w].phase = IF s[w].credit # {}
                                                  THEN "reviewed" ELSE @,
                                              ![w].credit = {},
                                              ![w].pending = FALSE,
                                              ![w].worker = WorkerId(w, 2),
                                              ![w].recovery = RecoveryId(w, 2),
                                              ![w].serviceOwner = IF ~s[w].failed
                                                  /\ s[w].phase \in
                                                  {"dispatched", "delivered"}
                                                  THEN ServiceId(2) ELSE Unowned]
Resume(w) == /\ s[w].canceled
             /\ (s[w].authority = s[w].generation \/ ~FenceResume)
             /\ s' = [s EXCEPT ![w].canceled = FALSE,
                                   ![w].badResume = s[w].authority # s[w].generation]
GrantResumeAuthority(w) == /\ s[w].canceled
                           /\ s[w].generation = 2
                           /\ s[w].authority # s[w].generation
                           /\ s' = [s EXCEPT ![w].authority = s[w].generation]
FaultBudgetAvailable(w) ==
    /\ s[w].failures < MaxFailures
    /\ (MaxFailures # 1 \/ \A x \in Works: s[x].failures = 0)
Crash(w) == /\ w \notin ReviewOnly
            /\ ~s[w].failed /\ FaultBudgetAvailable(w)
            /\ s[w].phase \notin {"new", "follow-up"}
            /\ s' = [s EXCEPT ![w].failed = TRUE,
                                  ![w].serviceOwner = Unowned,
                                  ![w].failedDuringOwnership = @ \/ s[w].phase \in
                                      {"dispatched", "delivered"},
                                  ![w].failures = @ + 1]
Recover(w) == /\ s[w].failed
              /\ s[w].recovery = RecoveryId(w, s[w].generation)
              /\ s' = [s EXCEPT ![w].failed = FALSE,
                                    ![w].recoveredBeforeEffect =
                                        @ \/ ~s[w].git,
                                    ![w].phase = IF s[w].phase \in
                                        {"dispatched", "delivered"}
                                        THEN "authorized" ELSE @,
                                    ![w].queued = IF s[w].routeMode = "queued"
                                        /\ s[w].phase \in {"dispatched", "delivered"}
                                        THEN TRUE ELSE @,
                                    ![w].serviceOwner = Unowned]
ReviewOnlyFailure(w) == /\ w \in ReviewOnly /\ ~s[w].failed
                        /\ FaultBudgetAvailable(w)
                        /\ s[w].phase \in {"authorized", "review-only-tested", "reviewed"}
                        /\ s' = [s EXCEPT ![w].failed = TRUE,
                                              ![w].failures = @ + 1]
FailWrite(w) == /\ w \notin ReviewOnly
                /\ ~s[w].ioFailed /\ FaultBudgetAvailable(w)
                /\ s[w].phase \in {"intent", "git", "follow-up-pending"}
                /\ s' = [s EXCEPT ![w].ioFailed = TRUE,
                                      ![w].failures = @ + 1]
RecoverIO(w) == /\ s[w].ioFailed
                /\ s' = [s EXCEPT ![w].ioFailed = FALSE,
                                      ![w].recoveredBeforeEffect =
                                          @ \/ ~s[w].git]
VersionTransition(w) == /\ s[w].version = 1 /\ ~s[w].badVersion
                        /\ (s[w].phase \in {"new", "follow-up", "review-only-complete"}
                            \/ s[w].canceled \/ ~GuardVersion)
                        /\ s' = [s EXCEPT ![w].badVersion =
                                      s[w].phase \notin {"new", "follow-up", "review-only-complete"}
                                      /\ ~s[w].canceled,
                                      ![w].version = 2]

Next == \E w \in Works:
    Intake(w) \/ AttemptCreate(w) \/ Authorize(w)
    \/ ChooseForeground(w) \/ Enqueue(w) \/ Dispatch(w)
    \/ Deliver(w) \/ Writer(w)
    \/ Review(w) \/ StaleEvidence(w) \/ UnrelatedEvidence(w)
    \/ (\E kind \in {"candidate", "requirement", "context", "scope"}:
           MisboundEvidence(w, kind))
    \/ TargetedReview(w) \/ RecheckSameCandidate(w) \/ CarryForward(w)
    \/ CorrectCandidate(w) \/ CanonicalResult(w, "pass")
    \/ (\E kind \in {"known", "new", "incomplete", "harness",
                       "leak", "unexplained", "misbound", "reference-fail"}:
           CanonicalResult(w, kind))
    \/ (\E kind \in {"untrusted", "misbound"}:
           PrewriteBaselineCorrupt(w, kind))
    \/ Tester(w) \/ LearnCapture(w)
    \/ LearnNoExpertise(w) \/ Ready(w) \/ LandingResult(w, "pass")
    \/ (\E kind \in {"known", "new", "incomplete", "harness", "leak",
                       "unexplained", "misbound", "reference-fail"}:
           LandingResult(w, kind))
    \/ LandingVerify(w)
    \/ (\E kind \in {"unchanged", "cleared", "untrusted", "misbound"}:
           TargetBaseline(w, kind))
    \/ ReviewOnlyStart(w) \/ ReviewOnlyReview(w) \/ ReviewOnlyReady(w)
    \/ Intent(w) \/ GitEffect(w) \/ PublishResult(w)
    \/ FollowUpStart(w) \/ FollowUp(w) \/ RetryEffect(w)
    \/ Observe(w) \/ Cleanup(w) \/ Cancel(w) \/ ReplaceGeneration(w)
    \/ GrantResumeAuthority(w) \/ Resume(w) \/ Crash(w)
    \/ ReviewOnlyFailure(w) \/ Recover(w) \/ FailWrite(w)
    \/ RecoverIO(w) \/ VersionTransition(w)
Spec == Init /\ [][Next]_vars
FairSpec == Spec /\ (\A w \in Works: WF_vars(Recover(w))
                                   /\ WF_vars(RecoverIO(w))
                                   /\ WF_vars(PublishResult(w)))

InputSafe == \A w \in Works:
    s[w].phase \notin {"written", "reviewed", "tested", "learned", "ready",
                       "verified", "intent", "git", "published", "follow-up-pending",
                       "follow-up", "review-only-tested", "review-only-complete"}
    \/ s[w].required \subseteq s[w].delivered
EvidenceSafe == \A w \in Works:
    s[w].phase \notin {"tested", "learned", "ready", "verified", "intent", "git",
                       "published", "follow-up-pending", "follow-up",
                       "review-only-complete"}
    \/ (s[w].credit = {CurrentEvidence(w, s[w].generation)}
        /\ s[w].credit \subseteq s[w].rawEvidence)
ResumeSafe == \A w \in Works: ~s[w].badResume
OneEffect == \A w \in Works: s[w].effects <= 1
ObservationSafe == \A w \in Works:
    s[w].observed # "bad"
CleanupSafe == \A w \in Works:
    ~s[w].cleaned \/ s[w].phase \in {"follow-up", "review-only-complete"}
VersionSafe == \A w \in Works: ~s[w].badVersion
PublicationSafe == \A w \in Works:
    (s[w].published => s[w].git) /\ (s[w].git => s[w].pending \/ s[w].published)
CancellationSafe == \A w \in Works: s[w].canceled => ~s[w].git
OwnershipSafe == \A w \in Works:
    s[w].phase \notin {"dispatched", "delivered"} \/ s[w].failed
    \/ s[w].serviceOwner = ServiceId(s[w].generation)
ServiceExclusive == \A w \in Works:
    s[w].serviceOwner # Unowned =>
        \A x \in Works \ {w}: s[x].serviceOwner = Unowned
RouteSafe == \A w \in Works:
    /\ (w \in ReviewOnly =>
        s[w].routeMode = "unset" /\ ~s[w].queued
        /\ s[w].serviceOwner = Unowned)
    /\ (s[w].queued =>
        s[w].routeMode = "queued" /\ s[w].phase = "authorized")
    /\ (s[w].phase \in {"dispatched", "delivered"} =>
        s[w].routeMode \in {"foreground", "queued"} /\ ~s[w].queued)
ServiceCompetitionBound == \A w \in Works:
    /\ s[w].phase \in {"new", "intake", "planned", "authorized",
                       "dispatched", "delivered", "written"}
    /\ s[w].observed = "none" /\ s[w].version = 1
    /\ ~s[w].cleaned /\ ~s[w].ioFailed
FoundationBound == \A w \in Works: s[w].generation = 1
\* Combine one failure with correction, carry-forward, and landing without
\* multiplying the search by unrelated dispatch and evidence alternatives.
CombinedBound == \A w \in Works:
    /\ s[w].generation = 1
    /\ s[w].routeMode \in {"unset", "foreground"}
    /\ s[w].reviewKind \in {"none", "fresh", "carried"}
    /\ ~(s[w].correction /\ s[w].rechecked)
    /\ (s[w].observed = "none" \/ s[w].phase = "follow-up"
        \/ (s[w].observedFailedWrite /\ s[w].failures = 1
            /\ s[w].observed = "intent"
            /\ ((s[w].pending /\ s[w].phase \in {"intent", "git"})
                \/ (s[w].published /\ s[w].phase \in
                    {"published", "follow-up-pending", "follow-up"}))))
    /\ (s[w].version = 1 \/ s[w].phase = "follow-up")
\* These bounds retain the applicable acceptance boundary while keeping
\* unrelated completion branches out of each guarded search.
GuardedCanonicalBound == \A w \in Works:
    /\ s[w].phase \in {"new", "intake", "planned", "authorized",
                       "dispatched", "delivered", "written", "reviewed", "tested"}
    /\ ~s[w].correction /\ ~s[w].rechecked
    /\ s[w].generation = 1 /\ s[w].observed = "none"
    /\ s[w].version = 1 /\ ~s[w].cleaned
GuardedLandingBound == \A w \in Works:
    /\ s[w].phase \in {"new", "intake", "planned", "authorized",
                       "dispatched", "delivered", "written", "reviewed",
                       "tested", "learned", "ready", "verified"}
    /\ ~s[w].correction /\ ~s[w].rechecked
    /\ s[w].generation = 1 /\ s[w].observed = "none"
    /\ s[w].version = 1 /\ ~s[w].cleaned
RecoveryBound == \A w \in Works:
    /\ ~s[w].correction /\ ~s[w].rechecked
    /\ s[w].routeMode \in {"unset", "foreground"}
    /\ s[w].observed = "none" /\ s[w].version = 1
    /\ ~s[w].cleaned
    /\ s[w].targetCommit = <<"target", w, 1>>
AcceptedBaselineSafe == \A w \in Works:
    s[w].phase \notin {"learned", "ready", "verified", "intent", "git",
                       "published", "follow-up-pending", "follow-up",
                       "review-only-complete"}
    \/ (s[w].acceptedCandidate = s[w].candidate
        /\ (w \in ReviewOnly \/
            (s[w].testedComplete /\ s[w].testedReferencePassed
             /\ s[w].prewriteTrusted
             /\ s[w].prewriteCommit = <<"source", w>>
             /\ s[w].testedFailures \subseteq s[w].prewriteFailures)))
KnownFailureUnreachable == \A w \in Works:
    ~(s[w].phase = "tested" /\ s[w].testedFailures = {<<"test", "known">>})
KnownLandingUnreachable == \A w \in Works:
    ~(s[w].phase = "verified" /\ s[w].landingFailures = {<<"test", "known">>})
CanonicalRejectionUnreachable == \A w \in Works:
    ~(s[w].phase = "reviewed" /\ s[w].canonicalOutcome # "unset"
      /\ ~TesterBaselineSafe(w))
UntrustedPrewriteRejectionUnreachable == \A w \in Works:
    ~(s[w].phase = "reviewed" /\ s[w].canonicalOutcome # "unset"
      /\ ~s[w].prewriteTrusted)
MisboundPrewriteRejectionUnreachable == \A w \in Works:
    ~(s[w].phase = "reviewed" /\ s[w].canonicalOutcome # "unset"
      /\ s[w].prewriteCommit # <<"source", w>>)
LandingRejectionUnreachable == \A w \in Works:
    ~(s[w].phase = "ready" /\ s[w].landingOutcome # "unset"
      /\ ~LandingBaselineSafe(w))
RecoveredPublicationUnreachable == \A w \in Works:
    ~(s[w].phase = "published" /\ s[w].failures = 1
      /\ s[w].recoveredBeforeEffect
      /\ ~s[w].failed /\ ~s[w].ioFailed)
RecoveredCorrectionUnreachable == \A w \in Works:
    ~(CombinedBound /\ s[w].correction /\ s[w].failures = 1
      /\ s[w].failedDuringOwnership /\ s[w].correctedAfterRecovery
      /\ s[w].phase = "reviewed"
      /\ ~s[w].failed /\ ~s[w].ioFailed)
InterruptedPendingObservationUnreachable == \A w \in Works:
    ~(CombinedBound /\ s[w].pending /\ s[w].ioFailed /\ s[w].failures = 1
      /\ s[w].observed = s[w].phase /\ s[w].observed = "intent")
ObservedRecoveryPublicationUnreachable == \A w \in Works:
    ~(s[w].observedFailedWrite /\ s[w].failures = 1
      /\ s[w].phase = "published" /\ ~s[w].ioFailed)
CombinedObservedRecoveryExcluded == CombinedBound /\ \A w \in Works:
    s[w].observedFailedWrite => s[w].ioFailed
LandingAcceptanceSafe == \A w \in Works:
    s[w].phase \notin {"verified", "intent", "git", "published",
                       "follow-up-pending", "follow-up"}
    \/ LandingBaselineSafe(w)
LearnerModeSafe == \A w \in Works:
    s[w].phase \notin {"learned", "ready", "verified", "intent", "git",
                       "published", "follow-up-pending", "follow-up"}
    \/ (s[w].learnerMode = "capture"
        /\ s[w].expertise = s[w].expertiseBeforeLearning + 1)
    \/ (s[w].learnerMode = "no-expertise"
        /\ s[w].expertise = s[w].expertiseBeforeLearning)
IdentitySafe == \A w \in Works:
    /\ s[w].attempt = AttemptId(w) /\ s[w].task = TaskId(w)
    /\ s[w].workspace = WorkspaceId(w)
    /\ (IF w \in ReviewOnly THEN s[w].candidate = "none"
        ELSE s[w].candidate \in {CandidateId(w), <<"candidate", w, 2>>})
    /\ s[w].dispatch = DispatchId(w)
    /\ s[w].worker = WorkerId(w, s[w].generation)
    /\ s[w].recovery = RecoveryId(w, s[w].generation)
    /\ s[w].context \in {ContextId(w), <<"context", w, 2>>}
    /\ s[w].reviewScope = (IF w \in ReviewOnly
        THEN <<"scope", w, "review-only">>
        ELSE <<"scope", w, s[w].candidate>>)
ReviewOnlySafe == \A w \in ReviewOnly:
    /\ ~s[w].git /\ ~s[w].pending /\ ~s[w].published
    /\ s[w].candidate = "none" /\ s[w].learnerMode = "unset"
    /\ s[w].expertise = 0
    /\ s[w].phase \notin {"dispatched", "delivered", "written", "tested",
                          "learned", "verified", "intent", "git", "published",
                          "follow-up-pending", "follow-up"}
ReviewOnlyNoGit == \A w \in ReviewOnly: ~s[w].git

\* Progress depends on a finite failure budget, enabled recovery, and a
\* continuing public scheduler/operator. FairSpec states only local recovery
\* and result-publication fairness; the later integrated check owns end to end
\* progress and human-decision fairness.
RecoveryProgress == \A w \in Works: s[w].failed ~> ~s[w].failed
=============================================================================
