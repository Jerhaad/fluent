-------------------- MODULE ReleaseOwnershipTrace --------------------
EXTENDS ReleaseOwnershipConnection
CONSTANTS BeforeHeldA, BeforeHeldB, AfterHeldA, AfterHeldB, CompletedA, CompletedB, NewOwner, StaleRefused
VARIABLE step
tracevars == <<vars, step>>
TraceInit == Init /\ step = 0
TraceNext == /\ CASE step = 0 -> RequestLegacyStop
                     [] step = 1 -> SettleA /\ (wa'["work"].phase = "written") = CompletedA
                     [] step = 2 -> SettleB /\ (wb'["work"].phase = "written") = CompletedB
                     [] step = 3 -> ReleaseLegacy
                     [] step = 4 -> StartCurrent
                     [] step = 5 -> StaleStop
                     [] step = 6 -> UNCHANGED vars
                     [] OTHER -> FALSE
             /\ step' = step + 1
TraceSpec == TraceInit /\ [][TraceNext]_tracevars /\ WF_tracevars(TraceNext)
Observed == /\ (step = 0 => <<aHeld, bHeld>> = <<BeforeHeldA, BeforeHeldB>>)
            /\ (step >= 3 => <<aHeld, bHeld>> = <<AfterHeldA, AfterHeldB>>)
            /\ (step >= 5 => (owner # 0) = NewOwner)
            /\ (step = 7 => (owner = 1 /\ ~staleStop) = StaleRefused)
TraceComplete == <> (step = 7)
=============================================================================
