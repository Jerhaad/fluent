------------------------ MODULE WriterTrace ------------------------
EXTENDS Workflow, Sequences

CONSTANTS WriterState1, WriterState2, WriterWork
WriterStates == <<WriterState1, WriterState2>>

VARIABLE writerIndex
writerVars == <<s, writerIndex>>
\* The classified snapshot and observed provider invocation establish the
\* dispatch precondition. Earlier fixture setup is outside this checked path.
WriterInit == /\ s = [w \in Works |->
                       [InitialWork(w) EXCEPT !.phase = "dispatched",
                                              !.routeMode = "foreground",
                                              !.serviceOwner = ServiceId(1)]]
              /\ writerIndex = 0
WriterAction ==
    CASE writerIndex = 0 -> Deliver(WriterWork)
      [] writerIndex = 1 -> Writer(WriterWork)
WriterNext == /\ writerIndex < Len(WriterStates)
              /\ WriterAction
              /\ writerIndex' = writerIndex + 1
              /\ s'[WriterWork].phase = WriterStates[writerIndex + 1]
WriterSpec == WriterInit /\ [][WriterNext]_writerVars /\ WF_writerVars(WriterNext)
WriterComplete == <> (writerIndex = Len(WriterStates))
=====================================================================
