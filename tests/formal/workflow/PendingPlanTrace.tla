------------------------- MODULE PendingPlanTrace -------------------------
EXTENDS Naturals, TLC

CONSTANTS Trace1work, Trace1attempt, Trace1candidate, Trace1writer,
          Trace1outputCommit, Trace1noChange, Trace1proofCount,
          Trace1proofComplete, Trace1testerSeen, Trace1ready,
          Trace2work, Trace2attempt, Trace2candidate, Trace2writer,
          Trace2outputCommit, Trace2noChange, Trace2proofCount,
          Trace2proofComplete, Trace2testerSeen, Trace2ready,
          Trace3work, Trace3attempt, Trace3candidate, Trace3writer,
          Trace3outputCommit, Trace3noChange, Trace3proofCount,
          Trace3proofComplete, Trace3testerSeen, Trace3ready,
          Trace4work, Trace4attempt, Trace4candidate, Trace4writer,
          Trace4outputCommit, Trace4noChange, Trace4proofCount,
          Trace4proofComplete, Trace4testerSeen, Trace4ready

VARIABLE state
vars == <<state>>

TraceState1 == [work |-> Trace1work, attempt |-> Trace1attempt,
                candidate |-> Trace1candidate, writer |-> Trace1writer,
                outputCommit |-> Trace1outputCommit,
                noChange |-> Trace1noChange, proofCount |-> Trace1proofCount,
                proofComplete |-> Trace1proofComplete,
                testerSeen |-> Trace1testerSeen, ready |-> Trace1ready]
TraceState2 == [work |-> Trace2work, attempt |-> Trace2attempt,
                candidate |-> Trace2candidate, writer |-> Trace2writer,
                outputCommit |-> Trace2outputCommit,
                noChange |-> Trace2noChange, proofCount |-> Trace2proofCount,
                proofComplete |-> Trace2proofComplete,
                testerSeen |-> Trace2testerSeen, ready |-> Trace2ready]
TraceState3 == [work |-> Trace3work, attempt |-> Trace3attempt,
                candidate |-> Trace3candidate, writer |-> Trace3writer,
                outputCommit |-> Trace3outputCommit,
                noChange |-> Trace3noChange, proofCount |-> Trace3proofCount,
                proofComplete |-> Trace3proofComplete,
                testerSeen |-> Trace3testerSeen, ready |-> Trace3ready]
TraceState4 == [work |-> Trace4work, attempt |-> Trace4attempt,
                candidate |-> Trace4candidate, writer |-> Trace4writer,
                outputCommit |-> Trace4outputCommit,
                noChange |-> Trace4noChange, proofCount |-> Trace4proofCount,
                proofComplete |-> Trace4proofComplete,
                testerSeen |-> Trace4testerSeen, ready |-> Trace4ready]

Init == state = TraceState1 /\ ~state.noChange /\ ~state.ready
NoChange == /\ ~state.noChange /\ state' = TraceState2
Attach == /\ state.noChange /\ state.proofCount = TraceState2.proofCount
          /\ state' = TraceState3
Resume == /\ state.proofCount = TraceState3.proofCount
          /\ ~state.ready /\ state' = TraceState4
Next == NoChange \/ Attach \/ Resume
Spec == Init /\ [][Next]_vars /\ WF_vars(NoChange) /\ WF_vars(Attach)
        /\ WF_vars(Resume)

TraceSafe == /\ state.work = TraceState1.work
             /\ state.attempt = TraceState1.attempt
             /\ state.candidate = TraceState1.candidate
             /\ state.outputCommit = state.candidate
             /\ (state.noChange => state.writer = TraceState2.writer)
             /\ (state.testerSeen => state.proofComplete)
             /\ (state.ready => state.noChange /\ state.proofComplete
                                /\ state.testerSeen
                                /\ state.proofCount > TraceState2.proofCount)
TraceComplete == <>state.ready
=============================================================================
