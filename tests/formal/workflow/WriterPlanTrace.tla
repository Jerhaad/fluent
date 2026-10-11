------------------------- MODULE WriterPlanTrace -------------------------
EXTENDS Naturals, TLC

CONSTANTS Trace1work, Trace1attempt, Trace1candidate, Trace1source, Trace1successor, Trace1pending, Trace1authorized, Trace1executed, Trace1outputCommit, Trace1hostPending, Trace1ready,
          Trace2work, Trace2attempt, Trace2candidate, Trace2source, Trace2successor, Trace2pending, Trace2authorized, Trace2executed, Trace2outputCommit, Trace2hostPending, Trace2ready,
          Trace3work, Trace3attempt, Trace3candidate, Trace3source, Trace3successor, Trace3pending, Trace3authorized, Trace3executed, Trace3outputCommit, Trace3hostPending, Trace3ready,
          Trace4work, Trace4attempt, Trace4candidate, Trace4source, Trace4successor, Trace4pending, Trace4authorized, Trace4executed, Trace4outputCommit, Trace4hostPending, Trace4ready

VARIABLES state, wf, replayed
vars == <<state, wf, replayed>>

W == INSTANCE Workflow WITH Works <- {"work"}, ReviewOnly <- {},
     MaxFailures <- 1, DeliverInputs <- TRUE, CheckEvidence <- TRUE,
     FenceResume <- TRUE, GuardEffect <- TRUE, TruthfulObservation <- TRUE,
     GuardCleanup <- TRUE, GuardVersion <- TRUE, GuardBaseline <- TRUE,
     GuardLandingBaseline <- TRUE, CanonicalKinds <- {"pass"},
     CorruptPrewrite <- FALSE, TargetKinds <- {"unchanged"},
     GuardReviewRoute <- TRUE, LandingKinds <- {"pass"}, s <- wf

TraceState1 == [work |-> Trace1work,
                attempt |-> Trace1attempt,
                candidate |-> Trace1candidate,
                source |-> Trace1source,
                successor |-> Trace1successor,
                pending |-> Trace1pending,
                authorized |-> Trace1authorized,
                executed |-> Trace1executed,
                outputCommit |-> Trace1outputCommit,
                hostPending |-> Trace1hostPending,
                ready |-> Trace1ready]
TraceState2 == [work |-> Trace2work,
                attempt |-> Trace2attempt,
                candidate |-> Trace2candidate,
                source |-> Trace2source,
                successor |-> Trace2successor,
                pending |-> Trace2pending,
                authorized |-> Trace2authorized,
                executed |-> Trace2executed,
                outputCommit |-> Trace2outputCommit,
                hostPending |-> Trace2hostPending,
                ready |-> Trace2ready]
TraceState3 == [work |-> Trace3work,
                attempt |-> Trace3attempt,
                candidate |-> Trace3candidate,
                source |-> Trace3source,
                successor |-> Trace3successor,
                pending |-> Trace3pending,
                authorized |-> Trace3authorized,
                executed |-> Trace3executed,
                outputCommit |-> Trace3outputCommit,
                hostPending |-> Trace3hostPending,
                ready |-> Trace3ready]
TraceState4 == [work |-> Trace4work,
                attempt |-> Trace4attempt,
                candidate |-> Trace4candidate,
                source |-> Trace4source,
                successor |-> Trace4successor,
                pending |-> Trace4pending,
                authorized |-> Trace4authorized,
                executed |-> Trace4executed,
                outputCommit |-> Trace4outputCommit,
                hostPending |-> Trace4hostPending,
                ready |-> Trace4ready]

\* The retained no-change Task is complete, but the Writer's required work is
\* incomplete. Model that logical obligation as an interrupted delivered phase.
Init == /\ state = TraceState1 /\ replayed = FALSE
        /\ wf = [w \in {"work"} |-> LET base == W!InitialWork(w) IN
             [base EXCEPT !.phase = "delivered", !.failed = TRUE]]
Authorize == /\ ~state.authorized /\ W!Recover("work")
             /\ state' = TraceState2 /\ UNCHANGED replayed
Run == /\ state.authorized /\ ~state.executed
       /\ CASE wf["work"].routeMode = "unset" -> /\ W!ChooseForeground("work") /\ UNCHANGED state
             [] wf["work"].phase = "authorized" -> /\ W!Dispatch("work") /\ UNCHANGED state
             [] wf["work"].phase = "dispatched" -> /\ W!Deliver("work") /\ UNCHANGED state
             [] OTHER -> /\ W!Writer("work") /\ state' = TraceState3
       /\ UNCHANGED replayed
Replay == /\ state.executed /\ ~replayed /\ state' = TraceState4
          /\ replayed' = TRUE /\ UNCHANGED wf
Next == Authorize \/ Run \/ Replay
Spec == Init /\ [][Next]_vars /\ WF_vars(Authorize) /\ WF_vars(Run)
        /\ WF_vars(Replay)

TraceSafe == /\ state.work = TraceState1.work
             /\ state.attempt = TraceState1.attempt
             /\ state.candidate = TraceState1.candidate
             /\ state.source = TraceState1.source
             /\ state.pending = ~state.executed
             /\ state.hostPending /\ ~state.ready
             /\ (state.authorized = ~wf["work"].failed)
             /\ (state.executed = (wf["work"].phase = "written"))
             /\ (state.authorized => state.successor = TraceState2.successor
                                      /\ state.successor # state.source)
             /\ (state.executed => state.authorized /\ wf["work"].delivered = wf["work"].required
                                    /\ state.outputCommit # state.candidate)
             /\ (~state.executed => state.outputCommit = state.candidate)
TraceComplete == <>replayed
=============================================================================
