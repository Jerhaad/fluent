--------------------------- MODULE ReviewConnection ---------------------------
EXTENDS Naturals, FiniteSets
CONSTANTS HonorTester, RetainTester
VARIABLES wf, tester, reviews, tested, crashed, stops
rvars == <<tester, reviews, tested, crashed, stops>>
vars == <<wf, rvars>>
W == INSTANCE Workflow WITH Works <- {"review"}, ReviewOnly <- {"review"},
     MaxFailures <- 2, DeliverInputs <- TRUE, CheckEvidence <- TRUE,
     FenceResume <- TRUE, GuardEffect <- TRUE, TruthfulObservation <- TRUE,
     GuardCleanup <- TRUE, GuardVersion <- TRUE, GuardBaseline <- TRUE,
     GuardLandingBaseline <- TRUE, CanonicalKinds <- {"pass"},
     CorruptPrewrite <- FALSE, TargetKinds <- {"unchanged"},
     GuardReviewRoute <- TRUE, LandingKinds <- {"pass"}, s <- wf

R == INSTANCE ReviewTasks WITH Roles <- {"architecture", "behaviors", "documentation", "skills", "tests"}
Init == /\ W!Init /\ R!Init
Prepare == /\ (W!Intake("review") \/ W!AttemptCreate("review") \/ W!Authorize("review"))
           /\ UNCHANGED rvars
Test == /\ R!Test /\ W!ReviewOnlyStart("review")
Start(r) == /\ ~wf["review"].canceled /\ R!Start(r) /\ UNCHANGED wf
Finish(r) == /\ ~wf["review"].canceled /\ R!Finish(r) /\ UNCHANGED wf
Credit == /\ R!Complete /\ W!ReviewOnlyReview("review") /\ UNCHANGED rvars
Ready == /\ W!ReviewOnlyReady("review") /\ UNCHANGED rvars
Crash == /\ R!Crash /\ W!ReviewOnlyFailure("review")
Cancel == /\ crashed /\ W!Cancel("review") /\ UNCHANGED rvars
Recover == /\ crashed /\ wf["review"].canceled /\ W!Recover("review") /\ UNCHANGED rvars
Resume == /\ crashed /\ ~wf["review"].failed /\ R!Restart /\ W!Resume("review")
StartReview == \E r \in DOMAIN reviews: Start(r)
FinishReview == \E r \in DOMAIN reviews: Finish(r)
Next == Prepare \/ Test \/ Credit \/ Ready \/ Crash \/ Cancel \/ Recover \/ Resume
        \/ StartReview \/ FinishReview
Spec == Init /\ [][Next]_vars
TesterFirst == R!TesterFirst
TesterRetained == R!TesterRetained
MappedReview == /\ (wf["review"].phase = "review-only-complete" => R!Complete)
                /\ (wf["review"].credit # {} => R!Complete)
                /\ W!ReviewOnlySafe
                /\ ~wf["review"].git
                /\ wf["review"].effects = 0
=============================================================================
