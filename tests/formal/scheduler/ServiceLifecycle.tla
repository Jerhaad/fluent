-------------------------- MODULE ServiceLifecycle --------------------------
EXTENDS Naturals, FiniteSets, TLC

CONSTANTS CheckBuild, BindInstance, GuardAdmission, ProtocolCompatible, DrainCapable,
          ExactReplay, RetainHistory
ASSUME CheckBuild \in BOOLEAN /\ BindInstance \in BOOLEAN /\ GuardAdmission \in BOOLEAN
       /\ ProtocolCompatible \in BOOLEAN /\ DrainCapable \in BOOLEAN
       /\ ExactReplay \in BOOLEAN /\ RetainHistory \in BOOLEAN
Instances == {1, 2}
Projects == {"A", "B"}

VARIABLES owner, lease, state, build, workers, queued, request, ack,
          result, crashUsed, badReuse, badAdmission, staleStop,
          nextFresh, operations, published, interrupted, replayInstance,
          replayOperation, replayResult, replayExpected, released
vars == <<owner, lease, state, build, workers, queued, request, ack,
          result, crashUsed, badReuse, badAdmission, staleStop,
          nextFresh, operations, published, interrupted, replayInstance,
          replayOperation, replayResult, replayExpected, released>>

Init == /\ owner = 0 /\ lease = 0 /\ state = "absent" /\ build = 0
        /\ workers = [p \in Projects |-> 0]
        /\ queued = [p \in Projects |-> 0]
        /\ request = 0 /\ ack = FALSE
        /\ result = [i \in Instances |-> "none"] /\ crashUsed = FALSE
        /\ nextFresh = 1 /\ operations = [i \in Instances |-> 0]
        /\ published = [i \in Instances |-> FALSE]
        /\ released = [i \in Instances |-> FALSE]
        /\ interrupted = [i \in Instances |-> FALSE]
        /\ replayInstance = 0 /\ replayOperation = 0
        /\ replayResult = "none" /\ replayExpected = "none"
        /\ badReuse = FALSE /\ badAdmission = FALSE /\ staleStop = FALSE

Start(i) == /\ i \in Instances /\ i = nextFresh /\ lease = 0
            /\ owner' = i /\ lease' = i /\ build' = i /\ state' = "running"
            /\ nextFresh' = i + 1
            /\ result' = IF RetainHistory \/ i = 1 THEN result
                          ELSE [result EXCEPT ![1] = "none"]
            /\ UNCHANGED <<workers, queued, request, ack, crashUsed, badReuse, badAdmission, staleStop, operations, published, interrupted, replayInstance, replayOperation, replayResult, replayExpected, released>>

Reuse(i) == /\ i \in Instances /\ lease = owner /\ owner # 0
            /\ state = "running" /\ ProtocolCompatible /\ DrainCapable
            /\ (CheckBuild => i = build)
            /\ badReuse' = (badReuse \/ (i # build))
            /\ UNCHANGED <<owner, lease, state, build, workers, queued, request, ack, result, crashUsed, badAdmission, staleStop, nextFresh, operations, published, interrupted, replayInstance, replayOperation, replayResult, replayExpected, released>>

Submit(p) == /\ p \in Projects /\ owner # 0 /\ queued[p] = 0
             /\ (GuardAdmission => state = "running")
             /\ queued' = [queued EXCEPT ![p] = 1]
             /\ badAdmission' = (badAdmission \/ (state = "draining"))
             /\ UNCHANGED <<owner, lease, state, build, workers, request, ack, result, crashUsed, badReuse, staleStop, nextFresh, operations, published, interrupted, replayInstance, replayOperation, replayResult, replayExpected, released>>

Claim(p) == /\ p \in Projects /\ owner # 0 /\ queued[p] = 1 /\ workers[p] = 0
            /\ (GuardAdmission => state = "running")
            /\ queued' = [queued EXCEPT ![p] = 0]
            /\ workers' = [workers EXCEPT ![p] = 1]
            /\ badAdmission' = (badAdmission \/ (state = "draining"))
            /\ UNCHANGED <<owner, lease, state, build, request, ack, result, crashUsed, badReuse, staleStop, nextFresh, operations, published, interrupted, replayInstance, replayOperation, replayResult, replayExpected, released>>

RequestStop(i) == /\ i \in Instances /\ owner # 0 /\ request = 0
                  /\ request' = i
                  /\ UNCHANGED <<owner, lease, state, build, workers, queued, ack, result, crashUsed, badReuse, badAdmission, staleStop, nextFresh, operations, published, interrupted, replayInstance, replayOperation, replayResult, replayExpected, released>>

Acknowledge == /\ request # 0 /\ owner # 0 /\ state = "running"
               /\ ProtocolCompatible /\ DrainCapable
               /\ (BindInstance => request = owner)
               /\ state' = "draining" /\ ack' = TRUE
               /\ operations' = [operations EXCEPT ![owner] = @ + 1]
               /\ result' = [result EXCEPT ![owner] = "draining"]
               /\ staleStop' = (staleStop \/ (request # owner))
               /\ UNCHANGED <<owner, lease, build, workers, queued, request, crashUsed, badReuse, badAdmission, nextFresh, published, interrupted, replayInstance, replayOperation, replayResult, replayExpected, released>>

Settle(p) == /\ p \in Projects /\ workers[p] = 1
             /\ workers' = [workers EXCEPT ![p] = 0]
             /\ UNCHANGED <<owner, lease, state, build, queued, request, ack, result, crashUsed, badReuse, badAdmission, staleStop, nextFresh, operations, published, interrupted, replayInstance, replayOperation, replayResult, replayExpected, released>>

Replay(i) == /\ i \in Instances /\ operations[i] = 1
             /\ replayInstance' = i
             /\ replayOperation' = operations[i]
             /\ replayResult' = IF ExactReplay THEN result[i] ELSE "wrong"
             /\ replayExpected' = result[i]
             /\ UNCHANGED <<owner, lease, state, build, workers, queued, request, ack, result, crashUsed, badReuse, badAdmission, staleStop, nextFresh, operations, published, interrupted, released>>

Publish == /\ state = "draining" /\ \A p \in Projects: workers[p] = 0
          /\ result' = [result EXCEPT ![owner] = "stopped"]
          /\ published' = [published EXCEPT ![owner] = TRUE]
          /\ state' = "published"
          /\ UNCHANGED <<owner, lease, build, workers, queued, request, ack, crashUsed, badReuse, badAdmission, staleStop, nextFresh, operations, interrupted, replayInstance, replayOperation, replayResult, replayExpected, released>>

Cleanup == /\ state = "published" /\ owner # 0
           /\ owner' = 0 /\ lease' = 0 /\ state' = "stopped"
           /\ request' = 0 /\ ack' = FALSE
           /\ released' = [released EXCEPT ![owner] = TRUE]
           /\ UNCHANGED <<build, workers, queued, result, crashUsed, badReuse, badAdmission, staleStop, nextFresh, operations, published, interrupted, replayInstance, replayOperation, replayResult, replayExpected>>

Crash == /\ ~crashUsed /\ owner # 0
         /\ crashUsed' = TRUE /\ owner' = 0 /\ lease' = 0 /\ state' = "absent"
         /\ request' = 0 /\ ack' = FALSE
         /\ workers' = [p \in Projects |-> 0]
         /\ released' = [released EXCEPT ![owner] = TRUE]
         /\ interrupted' = [interrupted EXCEPT ![owner] = (state = "draining")]
         /\ result' = IF state = "draining"
                       THEN [result EXCEPT ![owner] = "interrupted"]
                       ELSE result
         /\ UNCHANGED <<build, queued, badReuse, badAdmission, staleStop, nextFresh, operations, published, replayInstance, replayOperation, replayResult, replayExpected>>

Next == \/ \E i \in Instances: (Start(i) \/ Reuse(i) \/ RequestStop(i) \/ Replay(i))
        \/ \E p \in Projects: (Submit(p) \/ Claim(p) \/ Settle(p))
        \/ Acknowledge \/ Publish \/ Cleanup \/ Crash

Spec == Init /\ [][Next]_vars
FairSpec == Spec /\ WF_vars(Publish) /\ WF_vars(Cleanup)
            /\ (\A p \in Projects: WF_vars(Settle(p)))
DrainProgress(i) == (result[i] = "draining") ~>
                    (released[i] /\ (result[i] = "stopped" \/ interrupted[i]))
DrainProgressAll == \A i \in Instances: DrainProgress(i)
OneOwner == owner = lease \/ (owner = 0 /\ lease = 0)
NoMismatchedReuse == ~badReuse
NoAdmissionAfterDrain == ~badAdmission
NoStaleStop == ~staleStop
NoStoppedWithWorkers == state = "stopped" => \A p \in Projects: workers[p] = 0
NoLegacyDrain == (~ProtocolCompatible \/ ~DrainCapable) => state # "draining"
ReplayStable == /\ \A i \in Instances: operations[i] <= 1
                /\ (replayInstance # 0 => replayOperation = operations[replayInstance])
                /\ replayResult = replayExpected
RetainedResult == \A i \in Instances: published[i] => result[i] = "stopped"
NoEarlyRelease == state = "published" => owner = lease /\ owner # 0
=============================================================================
