-------------------------- MODULE ServiceLifecycle --------------------------
EXTENDS Naturals, FiniteSets, TLC

CONSTANTS CheckBuild, BindInstance, GuardAdmission, ProtocolCompatible, DrainCapable
ASSUME CheckBuild \in BOOLEAN /\ BindInstance \in BOOLEAN /\ GuardAdmission \in BOOLEAN
       /\ ProtocolCompatible \in BOOLEAN /\ DrainCapable \in BOOLEAN
Instances == {1, 2}
Projects == {"A", "B"}

VARIABLES owner, lease, state, build, workers, queued, request, ack,
          result, crashed, badReuse, badAdmission, staleStop
vars == <<owner, lease, state, build, workers, queued, request, ack,
          result, crashed, badReuse, badAdmission, staleStop>>

Init == /\ owner = 0 /\ lease = 0 /\ state = "absent" /\ build = 0
        /\ workers = [p \in Projects |-> 0]
        /\ queued = [p \in Projects |-> 0]
        /\ request = 0 /\ ack = FALSE /\ result = 0 /\ crashed = FALSE
        /\ badReuse = FALSE /\ badAdmission = FALSE /\ staleStop = FALSE

Start(i) == /\ i \in Instances /\ lease = 0
            /\ owner' = i /\ lease' = i /\ build' = i /\ state' = "running"
            /\ UNCHANGED <<workers, queued, request, ack, result, crashed,
                           badReuse, badAdmission, staleStop>>

Reuse(i) == /\ i \in Instances /\ lease = owner /\ owner # 0
            /\ state = "running" /\ ProtocolCompatible /\ DrainCapable
            /\ (CheckBuild => i = build)
            /\ badReuse' = (badReuse \/ (i # build))
            /\ UNCHANGED <<owner, lease, state, build, workers, queued,
                           request, ack, result, crashed, badAdmission, staleStop>>

Submit(p) == /\ p \in Projects /\ owner # 0 /\ queued[p] = 0
             /\ (GuardAdmission => state = "running")
             /\ queued' = [queued EXCEPT ![p] = 1]
             /\ badAdmission' = (badAdmission \/ (state = "draining"))
             /\ UNCHANGED <<owner, lease, state, build, workers, request,
                            ack, result, crashed, badReuse, staleStop>>

Claim(p) == /\ p \in Projects /\ owner # 0 /\ queued[p] = 1 /\ workers[p] = 0
            /\ (GuardAdmission => state = "running")
            /\ queued' = [queued EXCEPT ![p] = 0]
            /\ workers' = [workers EXCEPT ![p] = 1]
            /\ badAdmission' = (badAdmission \/ (state = "draining"))
            /\ UNCHANGED <<owner, lease, state, build, request, ack,
                           result, crashed, badReuse, staleStop>>

RequestStop(i) == /\ i \in Instances /\ owner # 0
                  /\ request' = i
                  /\ UNCHANGED <<owner, lease, state, build, workers, queued,
                                 ack, result, crashed, badReuse, badAdmission, staleStop>>

Acknowledge == /\ request # 0 /\ owner # 0 /\ state = "running"
               /\ ProtocolCompatible /\ DrainCapable
               /\ (BindInstance => request = owner)
               /\ state' = "draining" /\ ack' = TRUE
               /\ staleStop' = (staleStop \/ (request # owner))
               /\ UNCHANGED <<owner, lease, build, workers, queued, request,
                              result, crashed, badReuse, badAdmission>>

Settle(p) == /\ p \in Projects /\ workers[p] = 1
             /\ workers' = [workers EXCEPT ![p] = 0]
             /\ UNCHANGED <<owner, lease, state, build, queued, request,
                            ack, result, crashed, badReuse, badAdmission, staleStop>>

Finish == /\ state = "draining" /\ \A p \in Projects: workers[p] = 0
          /\ result' = owner /\ owner' = 0 /\ lease' = 0 /\ state' = "stopped"
          /\ UNCHANGED <<build, workers, queued, request, ack, crashed,
                         badReuse, badAdmission, staleStop>>

Crash == /\ ~crashed /\ owner # 0
         /\ crashed' = TRUE /\ owner' = 0 /\ lease' = 0 /\ state' = "absent"
         /\ workers' = [p \in Projects |-> 0]
         /\ UNCHANGED <<build, queued, request, ack, result,
                        badReuse, badAdmission, staleStop>>

Next == \/ \E i \in Instances: (Start(i) \/ Reuse(i) \/ RequestStop(i))
        \/ \E p \in Projects: (Submit(p) \/ Claim(p) \/ Settle(p))
        \/ Acknowledge \/ Finish \/ Crash

Spec == Init /\ [][Next]_vars
FairSpec == Spec /\ WF_vars(Finish) /\ (\A p \in Projects: WF_vars(Settle(p)))
DrainProgress == (state = "draining" /\ owner # 0) ~> (state = "stopped" \/ crashed)
OneOwner == owner = lease \/ (owner = 0 /\ lease = 0)
NoMismatchedReuse == ~badReuse
NoAdmissionAfterDrain == ~badAdmission
NoStaleStop == ~staleStop
NoStoppedWithWorkers == state = "stopped" => \A p \in Projects: workers[p] = 0
NoLegacyDrain == (~ProtocolCompatible \/ ~DrainCapable) => state # "draining"
ReplayStable == result # 0 => result \in Instances
=============================================================================
