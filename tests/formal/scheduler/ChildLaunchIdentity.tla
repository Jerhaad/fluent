------------------------- MODULE ChildLaunchIdentity -------------------------
EXTENDS Naturals, TLC

CONSTANTS RequireReceipt, CheckStop, CheckGeneration
ASSUME RequireReceipt \in BOOLEAN /\ CheckStop \in BOOLEAN /\ CheckGeneration \in BOOLEAN

VARIABLES generation, phase, held, receipt, parentAlive, stopped,
          badExecution, badStop, badSignal, executions
vars == <<generation, phase, held, receipt, parentAlive, stopped,
          badExecution, badStop, badSignal, executions>>

Init == /\ generation = 0 /\ phase = "idle" /\ held = FALSE
        /\ receipt = 0 /\ parentAlive = FALSE /\ stopped = FALSE
        /\ badExecution = FALSE /\ badStop = FALSE /\ badSignal = FALSE
        /\ executions = 0

Reserve == /\ phase \in {"idle", "settled"} /\ generation < 2 /\ ~held
           /\ generation' = generation + 1 /\ phase' = "reserved"
           /\ parentAlive' = TRUE /\ stopped' = FALSE
           /\ UNCHANGED <<held, receipt, badExecution, badStop, badSignal, executions>>

Create == /\ phase = "reserved" /\ ~stopped
          /\ phase' = "created" /\ held' = TRUE
          /\ UNCHANGED <<generation, receipt, parentAlive, stopped,
                        badExecution, badStop, badSignal, executions>>

ChildReceipt == /\ phase = "created" /\ held
                /\ receipt' = generation /\ phase' = "receipted"
                /\ UNCHANGED <<generation, held, parentAlive, stopped,
                              badExecution, badStop, badSignal, executions>>

ParentPublish == /\ phase = "receipted" /\ parentAlive
                 /\ phase' = "published"
                 /\ UNCHANGED <<generation, held, receipt, parentAlive, stopped,
                               badExecution, badStop, badSignal, executions>>

Execute == /\ phase \in {"created", "receipted", "published"} /\ held
           /\ (RequireReceipt => receipt = generation)
           /\ (CheckStop => ~stopped)
           /\ phase' = "executing" /\ executions' = executions + 1
           /\ badExecution' = badExecution \/ (receipt # generation)
           /\ badStop' = badStop \/ stopped
           /\ UNCHANGED <<generation, held, receipt, parentAlive, stopped, badSignal>>

Crash == /\ parentAlive /\ phase \in {"created", "receipted", "published", "executing"}
         /\ parentAlive' = FALSE
         /\ UNCHANGED <<generation, phase, held, receipt, stopped,
                       badExecution, badStop, badSignal, executions>>

RequestStop == /\ held /\ ~stopped
               /\ stopped' = TRUE
               /\ UNCHANGED <<generation, phase, held, receipt, parentAlive,
                             badExecution, badStop, badSignal, executions>>

Recover == /\ held /\ ~parentAlive /\ receipt = generation
           /\ held' = FALSE /\ phase' = "settled"
           /\ UNCHANGED <<generation, receipt, parentAlive, stopped,
                         badExecution, badStop, badSignal, executions>>

Signal == /\ held /\ stopped /\ receipt # 0
          /\ (CheckGeneration => receipt = generation)
          /\ badSignal' = badSignal \/ (receipt # generation)
          /\ held' = FALSE /\ phase' = "settled"
          /\ UNCHANGED <<generation, receipt, parentAlive, stopped,
                        badExecution, badStop, executions>>

Settle == /\ held /\ phase = "executing"
          /\ held' = FALSE /\ phase' = "settled"
          /\ UNCHANGED <<generation, receipt, parentAlive, stopped,
                        badExecution, badStop, badSignal, executions>>

Next == Reserve \/ Create \/ ChildReceipt \/ ParentPublish \/ Execute
        \/ Crash \/ RequestStop \/ Recover \/ Signal \/ Settle
Spec == Init /\ [][Next]_vars
FairSpec == Spec /\ WF_vars(Recover) /\ WF_vars(Settle)

NoUnrecordedExecution == ~badExecution
StopBeforeExecution == ~badStop
NoWrongSignal == ~badSignal
OwnedExecution == phase = "executing" => (held /\ receipt = generation)
RecoveryProgress == (held /\ ~parentAlive /\ receipt = generation) ~> ~held
=============================================================================
