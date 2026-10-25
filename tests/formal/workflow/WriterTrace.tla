------------------------ MODULE WriterTrace ------------------------
EXTENDS Workflow, Sequences

CONSTANTS WriterState1, WriterState2, WriterWork
WriterStates == <<WriterState1, WriterState2>>

VARIABLE writerIndex
writerVars == <<s, writerIndex>>
\* The classified snapshot provides the current human authorization. The
\* public Attempt run invokes the same Task before it reads its required input.
WriterInit == /\ s = [w \in Works |->
                       [InitialWork(w) EXCEPT !.phase = "planned",
                                              !.routeMode = "unset",
                                              !.serviceOwner = Unowned]]
              /\ writerIndex = 0
WriterAction ==
    CASE writerIndex = 0 -> Deliver(WriterWork)
      [] writerIndex = 1 -> Writer(WriterWork)
WriterPrepare == /\ writerIndex = 0
                 /\ (Authorize(WriterWork) \/ ChooseForeground(WriterWork) \/ Dispatch(WriterWork))
                 /\ UNCHANGED writerIndex
WriterNext == /\ writerIndex < Len(WriterStates)
              /\ WriterAction
              /\ writerIndex' = writerIndex + 1
              /\ s'[WriterWork].phase = WriterStates[writerIndex + 1]
WriterSpec == WriterInit /\ [][WriterPrepare \/ WriterNext]_writerVars
              /\ WF_writerVars(WriterPrepare) /\ WF_writerVars(WriterNext)
WriterComplete == <> (writerIndex = Len(WriterStates))
=====================================================================
