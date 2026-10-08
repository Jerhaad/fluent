---- MODULE ServiceIsolation ----
EXTENDS Naturals, TLC
CONSTANTS IsolateEnabled, SpinEnabled, RetainWorkers
VARIABLES polls, retries, good, badWorker, goodWorker
vars == <<polls, retries, good, badWorker, goodWorker>>
Init == /\ polls = 0
        /\ retries = 0
        /\ good = "queued"
        /\ badWorker = "running"
        /\ goodWorker = "running"
Poll == /\ polls < 2
        /\ polls' = polls + 1
        /\ retries' = retries + 1
        /\ good' = IF IsolateEnabled THEN "running" ELSE good
        /\ UNCHANGED <<badWorker, goodWorker>>
Spin == /\ SpinEnabled
        /\ retries < 3
        /\ retries' = retries + 1
        /\ UNCHANGED <<polls, good, badWorker, goodWorker>>
SettleBad == /\ badWorker = "running"
             /\ badWorker' = "settled"
             /\ goodWorker' = IF RetainWorkers THEN goodWorker ELSE "lost"
             /\ UNCHANGED <<polls, retries, good>>
SettleGood == /\ goodWorker = "running"
              /\ goodWorker' = "settled"
              /\ good' = "done"
              /\ UNCHANGED <<polls, retries, badWorker>>
Next == Poll \/ Spin \/ SettleBad \/ SettleGood
Spec == Init /\ [][Next]_vars
HealthyProgress == polls > 0 => good # "queued"
PollBound == retries <= polls
WorkerRetained == goodWorker # "lost"
====
