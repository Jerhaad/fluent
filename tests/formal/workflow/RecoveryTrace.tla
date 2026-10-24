-------------------------- MODULE RecoveryTrace --------------------------
EXTENDS Naturals, TLC

CONSTANTS Trace1attemptStatus, Trace1work, Trace1attempt, Trace1source,
          Trace1candidate, Trace1correction, Trace1correctionStatus,
          Trace1operation, Trace1authority, Trace1outputCommit, Trace1published,
          Trace2attemptStatus, Trace2work, Trace2attempt, Trace2source,
          Trace2candidate, Trace2correction, Trace2correctionStatus,
          Trace2operation, Trace2authority, Trace2outputCommit, Trace2published,
          Trace3attemptStatus, Trace3work, Trace3attempt, Trace3source,
          Trace3candidate, Trace3correction, Trace3correctionStatus,
          Trace3operation, Trace3authority, Trace3outputCommit, Trace3published
VARIABLES state, wf
vars == <<state, wf>>

W == INSTANCE Workflow WITH Works <- {"work"}, ReviewOnly <- {},
     MaxFailures <- 0, DeliverInputs <- TRUE, CheckEvidence <- TRUE,
     FenceResume <- TRUE, GuardEffect <- TRUE, TruthfulObservation <- TRUE,
     GuardCleanup <- TRUE, GuardVersion <- TRUE, GuardBaseline <- TRUE,
     GuardLandingBaseline <- TRUE, CanonicalKinds <- {"pass"},
     CorruptPrewrite <- FALSE, TargetKinds <- {"unchanged"},
     GuardReviewRoute <- TRUE, LandingKinds <- {"pass"}, s <- wf


TraceState1 == [attemptStatus |-> Trace1attemptStatus, work |-> Trace1work,
                attempt |-> Trace1attempt, source |-> Trace1source,
                candidate |-> Trace1candidate, correction |-> Trace1correction,
                correctionStatus |-> Trace1correctionStatus,
                operation |-> Trace1operation, authority |-> Trace1authority,
                outputCommit |-> Trace1outputCommit,
                published |-> Trace1published]
TraceState2 == [attemptStatus |-> Trace2attemptStatus, work |-> Trace2work,
                attempt |-> Trace2attempt, source |-> Trace2source,
                candidate |-> Trace2candidate, correction |-> Trace2correction,
                correctionStatus |-> Trace2correctionStatus,
                operation |-> Trace2operation, authority |-> Trace2authority,
                outputCommit |-> Trace2outputCommit,
                published |-> Trace2published]
TraceState3 == [attemptStatus |-> Trace3attemptStatus, work |-> Trace3work,
                attempt |-> Trace3attempt, source |-> Trace3source,
                candidate |-> Trace3candidate, correction |-> Trace3correction,
                correctionStatus |-> Trace3correctionStatus,
                operation |-> Trace3operation, authority |-> Trace3authority,
                outputCommit |-> Trace3outputCommit,
                published |-> Trace3published]

Identity(s) == /\ s.work = TraceState1.work
               /\ s.attempt = TraceState1.attempt
               /\ s.source = TraceState1.source
               /\ s.candidate = TraceState1.candidate
Authority(s) == /\ s.operation # "absent"
                /\ s.authority # "absent"
                /\ s.correction # "absent"
Init == /\ wf = [w \in {"work"} |-> LET base == W!InitialWork(w) IN
             [base EXCEPT !.phase = "delivered", !.delivered = base.required,
                          !.serviceOwner = W!ServiceId(1), !.routeMode = "foreground"]]
        /\ state = TraceState1 /\ state.attemptStatus = "needs-user"
        /\ state.correctionStatus = "absent"
        /\ state.operation = "absent" /\ state.outputCommit = "absent"
        /\ ~state.published
Repair == /\ UNCHANGED wf
          /\ state.correctionStatus = "absent"
          /\ state' = TraceState2
          /\ state'.attemptStatus = "planned"
          /\ Identity(state') /\ Authority(state')
          /\ state'.correctionStatus = "planned"
          /\ state'.outputCommit = "absent"
          /\ ~state'.published
Run == /\ W!Writer("work")
       /\ state.correctionStatus = "planned"
       /\ Authority(state)
       /\ state' = TraceState3
       /\ state'.attemptStatus = "needs-user"
       /\ Identity(state')
       /\ state'.operation = state.operation
       /\ state'.authority = state.authority
       /\ state'.correction = state.correction
       /\ state'.correctionStatus = "complete"
       /\ state'.published
Next == Repair \/ Run
Spec == Init /\ [][Next]_vars /\ WF_vars(Repair) /\ WF_vars(Run)
TraceComplete == <> (state.correctionStatus = "complete")
TraceSafe == /\ (state.published = (wf["work"].phase = "written"))
             /\ Identity(state)
             /\ (state.correctionStatus # "absent" => Authority(state))
             /\ (state.published => state.correctionStatus = "complete"
                                    /\ state.outputCommit = state.candidate)
=============================================================================
