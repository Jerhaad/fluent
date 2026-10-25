-------------------------- MODULE ServiceIsolation -------------------------
EXTENDS Naturals, FiniteSets

CONSTANT ContainFailure
Projects == {1, 2}
VARIABLES readable, running, cursor, completed, blocked
vars == <<readable, running, cursor, completed, blocked>>

Init ==
    /\ readable = [p \in Projects |-> p = 2]
    /\ running = TRUE
    /\ cursor = 1
    /\ completed = {}
    /\ blocked = {}

TypeOK ==
    /\ readable \in [Projects -> BOOLEAN]
    /\ running \in BOOLEAN
    /\ cursor \in 1..3
    /\ completed \subseteq Projects
    /\ blocked \subseteq Projects

PollProject ==
    /\ running
    /\ cursor \in Projects
    /\ IF readable[cursor]
          THEN /\ completed' = completed \cup {cursor}
               /\ blocked' = blocked \ {cursor}
               /\ cursor' = cursor + 1
               /\ UNCHANGED running
          ELSE IF ContainFailure
                 THEN /\ blocked' = blocked \cup {cursor}
                      /\ cursor' = cursor + 1
                      /\ UNCHANGED <<running, completed>>
                 ELSE /\ running' = FALSE
                      /\ UNCHANGED <<cursor, completed, blocked>>
    /\ UNCHANGED readable

NextPoll ==
    /\ running
    /\ cursor = 3
    /\ cursor' = 1
    /\ UNCHANGED <<readable, running, completed, blocked>>

Restart ==
    /\ ~running
    /\ running' = TRUE
    /\ cursor' = 1
    /\ UNCHANGED <<readable, completed, blocked>>

Repair ==
    /\ ~readable[1]
    /\ readable' = [readable EXCEPT ![1] = TRUE]
    /\ UNCHANGED <<running, cursor, completed, blocked>>

Next == PollProject \/ NextPoll \/ Restart \/ Repair
Spec == Init /\ [][Next]_vars
        /\ WF_vars(PollProject) /\ WF_vars(NextPoll) /\ WF_vars(Restart)

\* Repair is optional: healthy work must finish even if another project stays bad.
HealthyProgress == <>(2 \in completed)
RepairedProgress == readable[1] ~> (1 \in completed)
=============================================================================
