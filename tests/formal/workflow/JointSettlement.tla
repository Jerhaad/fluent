------------------------- MODULE JointSettlement -------------------------
EXTENDS Naturals, TLC

CONSTANT GuardSettlement
ASSUME GuardSettlement \in BOOLEAN

\* ServiceLifecycle counts workers; ChildLaunchIdentity owns the child receipt.
\* Keep the receipt and the unsettled child across a service crash so that a
\* restarted service cannot publish a completed stop before child settlement.
VARIABLES generation, service, child, receipt, stop, published, crashes
vars == <<generation, service, child, receipt, stop, published, crashes>>

Init == /\ generation = 1 /\ service = "running" /\ child = "absent"
        /\ receipt = 0 /\ stop = FALSE /\ published = FALSE /\ crashes = 0
Launch == /\ service = "running" /\ child = "absent" /\ ~stop
          /\ child' = "owned" /\ receipt' = generation
          /\ UNCHANGED <<generation, service, stop, published, crashes>>
RequestStop == /\ child = "owned" /\ ~stop
               /\ stop' = TRUE
               /\ UNCHANGED <<generation, service, child, receipt, published, crashes>>
Crash == /\ service = "running" /\ crashes < 2
         /\ service' = "absent" /\ crashes' = crashes + 1
         /\ UNCHANGED <<generation, child, receipt, stop, published>>
Restart == /\ service = "absent" /\ generation < 3
           /\ service' = "running" /\ generation' = generation + 1
           /\ UNCHANGED <<child, receipt, stop, published, crashes>>
Settle == /\ child = "owned" /\ stop /\ receipt > 0
          /\ child' = "settled"
          /\ UNCHANGED <<generation, service, receipt, stop, published, crashes>>
Publish == /\ service = "running" /\ stop /\ ~published
           /\ (GuardSettlement => child = "settled")
           /\ published' = TRUE
           /\ UNCHANGED <<generation, service, child, receipt, stop, crashes>>
Next == Launch \/ RequestStop \/ Crash \/ Restart \/ Settle \/ Publish
Spec == Init /\ [][Next]_vars
NoEarlyPublication == published => child = "settled"
ReceiptRetained == child = "owned" => receipt > 0
CrashRetainsChild == service = "absent" /\ child = "owned" => receipt > 0
RepeatedCrashUnreachable == crashes < 2
=============================================================================
