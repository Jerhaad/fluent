-------------------------- MODULE RecoveryTrace --------------------------
EXTENDS Naturals, TLC

CONSTANTS Trace1phase, Trace1work, Trace1attempt, Trace1source,
          Trace1candidate, Trace1correction, Trace1correctionStatus,
          Trace1operation, Trace1authority, Trace1published,
          Trace2phase, Trace2work, Trace2attempt, Trace2source,
          Trace2candidate, Trace2correction, Trace2correctionStatus,
          Trace2operation, Trace2authority, Trace2published,
          Trace3phase, Trace3work, Trace3attempt, Trace3source,
          Trace3candidate, Trace3correction, Trace3correctionStatus,
          Trace3operation, Trace3authority, Trace3published
VARIABLE state
vars == <<state>>

TraceState1 == [phase |-> Trace1phase, work |-> Trace1work,
                attempt |-> Trace1attempt, source |-> Trace1source,
                candidate |-> Trace1candidate, correction |-> Trace1correction,
                correctionStatus |-> Trace1correctionStatus,
                operation |-> Trace1operation, authority |-> Trace1authority,
                published |-> Trace1published]
TraceState2 == [phase |-> Trace2phase, work |-> Trace2work,
                attempt |-> Trace2attempt, source |-> Trace2source,
                candidate |-> Trace2candidate, correction |-> Trace2correction,
                correctionStatus |-> Trace2correctionStatus,
                operation |-> Trace2operation, authority |-> Trace2authority,
                published |-> Trace2published]
TraceState3 == [phase |-> Trace3phase, work |-> Trace3work,
                attempt |-> Trace3attempt, source |-> Trace3source,
                candidate |-> Trace3candidate, correction |-> Trace3correction,
                correctionStatus |-> Trace3correctionStatus,
                operation |-> Trace3operation, authority |-> Trace3authority,
                published |-> Trace3published]

Identity(s) == /\ s.work = TraceState1.work
               /\ s.attempt = TraceState1.attempt
               /\ s.source = TraceState1.source
               /\ s.candidate = TraceState1.candidate
Authority(s) == /\ s.operation # "absent"
                /\ s.authority # "absent"
                /\ s.correction # "absent"
Init == state = TraceState1 /\ state.phase = "paused"
        /\ state.correctionStatus = "absent"
        /\ state.operation = "absent" /\ ~state.published
Repair == /\ state.phase = "paused"
          /\ state' = TraceState2
          /\ state'.phase = "authorized"
          /\ Identity(state') /\ Authority(state')
          /\ state'.correctionStatus = "planned"
          /\ ~state'.published
Run == /\ state.phase = "authorized"
       /\ Authority(state)
       /\ state' = TraceState3
       /\ state'.phase = "continued"
       /\ Identity(state')
       /\ state'.operation = state.operation
       /\ state'.authority = state.authority
       /\ state'.correction = state.correction
       /\ state'.correctionStatus = "complete"
       /\ state'.published
Next == Repair \/ Run
Spec == Init /\ [][Next]_vars /\ WF_vars(Repair) /\ WF_vars(Run)
TraceComplete == <> (state.phase = "continued")
TraceSafe == /\ Identity(state)
             /\ (state.phase # "paused" => Authority(state))
             /\ (state.published => state.correctionStatus = "complete")
=============================================================================
