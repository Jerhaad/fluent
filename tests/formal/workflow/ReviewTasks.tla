---------------------------- MODULE ReviewTasks ----------------------------
EXTENDS Naturals, FiniteSets
CONSTANTS Roles, HonorTester, RetainTester
VARIABLES tester, reviews, tested, crashed, stops
vars == <<tester, reviews, tested, crashed, stops>>
Init == /\ tester = "planned" /\ reviews = [r \in Roles |-> "planned"]
        /\ tested = FALSE /\ crashed = FALSE /\ stops = 0
Test == /\ tester = "planned" /\ tester' = "complete" /\ tested' = TRUE
        /\ UNCHANGED <<reviews, crashed, stops>>
Start(r) == /\ reviews[r] = "planned" /\ ~crashed
            /\ (HonorTester => tester = "complete")
            /\ reviews' = [reviews EXCEPT ![r] = "executing"]
            /\ UNCHANGED <<tester, tested, crashed, stops>>
Finish(r) == /\ reviews[r] = "executing" /\ ~crashed
             /\ reviews' = [reviews EXCEPT ![r] = "complete"]
             /\ UNCHANGED <<tester, tested, crashed, stops>>
Crash == /\ ~crashed /\ stops < 2
         /\ \E r \in Roles: reviews[r] = "executing"
         /\ crashed' = TRUE /\ stops' = stops + 1
         /\ UNCHANGED <<tester, reviews, tested>>
Restart == /\ crashed /\ crashed' = FALSE
           /\ reviews' = [r \in Roles |-> IF reviews[r] = "executing" THEN "planned" ELSE reviews[r]]
           /\ tester' = IF RetainTester THEN tester ELSE "planned"
           /\ UNCHANGED <<tested, stops>>
TesterFirst == \A r \in Roles: reviews[r] # "planned" => tester = "complete"
TesterRetained == tested => tester = "complete"
Complete == \A r \in Roles: reviews[r] = "complete"
=============================================================================
