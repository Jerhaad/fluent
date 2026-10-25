---------------------------- MODULE Trace ----------------------------
EXTENDS Workflow, Sequences

CONSTANTS TraceState1, TraceState2, TraceState3, TraceState4, TraceWork
TraceStates == <<TraceState1, TraceState2, TraceState3, TraceState4>>

VARIABLE traceIndex
traceVars == <<s, traceIndex>>
TraceInit == Init /\ traceIndex = 0
TraceAction ==
    CASE traceIndex = 0 -> Intake(TraceWork)
      [] traceIndex = 1 -> AttemptCreate(TraceWork)
      [] traceIndex = 2 -> Cancel(TraceWork)
      [] traceIndex = 3 -> s[TraceWork].canceled /\ UNCHANGED s
TraceNext == /\ traceIndex < Len(TraceStates)
             /\ TraceAction
             /\ traceIndex' = traceIndex + 1
             /\ (IF s'[TraceWork].canceled THEN "canceled"
                 ELSE s'[TraceWork].phase) = TraceStates[traceIndex + 1]
TraceSpec == TraceInit /\ [][TraceNext]_traceVars /\ WF_traceVars(TraceNext)
TraceComplete == <> (traceIndex = Len(TraceStates))
======================================================================
