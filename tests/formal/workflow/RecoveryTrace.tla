-------------------------- MODULE RecoveryTrace --------------------------
EXTENDS Naturals, TLC

CONSTANTS TraceState1, TraceState2, TraceState3
VARIABLES phase, traceIndex
vars == <<phase, traceIndex>>

Init == phase = "paused" /\ traceIndex = 1 /\ phase = TraceState1
Repair == /\ phase = "paused" /\ traceIndex = 1
          /\ phase' = "authorized" /\ traceIndex' = 2
          /\ phase' = TraceState2
Run == /\ phase = "authorized" /\ traceIndex = 2
       /\ phase' = "continued" /\ traceIndex' = 3
       /\ phase' = TraceState3
Next == Repair \/ Run
Spec == Init /\ [][Next]_vars /\ WF_vars(Repair) /\ WF_vars(Run)
TraceComplete == <> (traceIndex = 3)
TraceSafe == traceIndex = 3 => phase = "continued"
=============================================================================
