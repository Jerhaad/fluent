------------------------ MODULE WriterTrace ------------------------
EXTENDS Workflow, Sequences

CONSTANTS WriterState1, WriterState2, WriterState3, WriterState4,
          WriterState5, WriterState6, WriterState7, WriterWork
WriterStates == <<WriterState1, WriterState2, WriterState3, WriterState4,
                  WriterState5, WriterState6, WriterState7>>

VARIABLE writerIndex
writerVars == <<s, writerIndex>>
WriterInit == Init /\ writerIndex = 0
WriterAction ==
    CASE writerIndex = 0 -> Intake(WriterWork)
      [] writerIndex = 1 -> AttemptCreate(WriterWork)
      [] writerIndex = 2 -> Authorize(WriterWork)
      [] writerIndex = 3 -> ChooseForeground(WriterWork)
      [] writerIndex = 4 -> Dispatch(WriterWork)
      [] writerIndex = 5 -> Deliver(WriterWork)
      [] writerIndex = 6 -> Writer(WriterWork)
WriterNext == /\ writerIndex < Len(WriterStates)
              /\ WriterAction
              /\ writerIndex' = writerIndex + 1
              /\ s'[WriterWork].phase = WriterStates[writerIndex + 1]
WriterSpec == WriterInit /\ [][WriterNext]_writerVars /\ WF_writerVars(WriterNext)
WriterComplete == <> (writerIndex = Len(WriterStates))
=====================================================================
