------------------------ MODULE LandingPublication ------------------------
EXTENDS Naturals, TLC

CONSTANTS Candidates, Actors, BindIntent, EarlyReconcile, ReplayProtection
ASSUME Candidates # {} /\ Actors = {"writer", "retry"}
       /\ BindIntent \in BOOLEAN /\ EarlyReconcile \in BOOLEAN
       /\ ReplayProtection \in BOOLEAN

VARIABLES owner, crashes, failures, intent, target, result, provenance, cleaned,
          stopped, ioFailed, effects, independent, badUnbound, badReplay
vars == <<owner, crashes, failures, intent, target, result, provenance, cleaned,
          stopped, ioFailed, effects, independent, badUnbound, badReplay>>

Init == /\ owner = "writer" /\ crashes = 0 /\ failures = 0
        /\ intent = "none" /\ target = "base" /\ result = "none"
        /\ provenance = "writer" /\ cleaned = FALSE /\ stopped = FALSE
        /\ ioFailed = FALSE /\ effects = 0 /\ independent = FALSE
        /\ badUnbound = FALSE /\ badReplay = FALSE

PublishIntent == \E candidate \in Candidates:
                 /\ owner = "writer" /\ intent = "none" /\ ~stopped /\ ~ioFailed
                 /\ intent' = candidate /\ provenance' = candidate
                 /\ UNCHANGED <<owner, crashes, failures, target, result, cleaned, stopped,
                               ioFailed, effects, independent, badUnbound, badReplay>>

GitEffect == /\ target = "base" /\ ~stopped
             /\ (intent \in Candidates \/ ~BindIntent)
             /\ target' = IF intent \in Candidates THEN intent
                           ELSE CHOOSE candidate \in Candidates: TRUE
             /\ effects' = effects + 1
             /\ badUnbound' = (badUnbound \/ (intent \notin Candidates))
             /\ UNCHANGED <<owner, crashes, failures, intent, result, provenance, cleaned,
                           stopped, ioFailed, independent, badReplay>>

Advance == /\ target \in Candidates /\ target' = "later"
           /\ UNCHANGED <<owner, crashes, failures, intent, result, provenance, cleaned,
                         stopped, ioFailed, effects, independent, badUnbound, badReplay>>

PublishResult == /\ intent \in Candidates /\ target \in {intent, "later"}
                 /\ result = "none" /\ ~ioFailed
                 /\ result' = intent
                 /\ UNCHANGED <<owner, crashes, failures, intent, target, provenance, cleaned,
                               stopped, ioFailed, effects, independent, badUnbound, badReplay>>

IndependentWrite == /\ result \in Candidates /\ ~independent
                    /\ independent' = TRUE
                    /\ UNCHANGED <<owner, crashes, failures, intent, target, result, provenance,
                                  cleaned, stopped, ioFailed, effects, badUnbound, badReplay>>

Crash == /\ owner = "writer" /\ crashes < 1 /\ intent \in Candidates
         /\ owner' = "retry" /\ crashes' = crashes + 1
         /\ UNCHANGED <<failures, intent, target, result, provenance, cleaned, stopped,
                       ioFailed, effects, independent, badUnbound, badReplay>>

Retry == /\ owner = "retry" /\ intent \in Candidates
         /\ target \in {intent, "later"} /\ result = intent
         /\ effects' = IF EarlyReconcile THEN effects ELSE effects + 1
         /\ independent' = IF ReplayProtection THEN independent ELSE FALSE
         /\ badReplay' = (badReplay \/ (independent /\ ~ReplayProtection))
         /\ UNCHANGED <<owner, crashes, failures, intent, target, result, provenance,
                       cleaned, stopped, ioFailed, badUnbound>>

Cleanup == /\ result \in Candidates /\ ~cleaned /\ cleaned' = TRUE
           /\ UNCHANGED <<owner, crashes, failures, intent, target, result, provenance,
                         stopped, ioFailed, effects, independent, badUnbound, badReplay>>

Cancel == /\ ~stopped /\ target = "base" /\ stopped' = TRUE
          /\ UNCHANGED <<owner, crashes, failures, intent, target, result, provenance,
                        cleaned, ioFailed, effects, independent, badUnbound, badReplay>>

FailWrite == /\ ~ioFailed /\ failures < 1
             /\ ioFailed' = TRUE /\ failures' = failures + 1
             /\ UNCHANGED <<owner, crashes, intent, target, result, provenance,
                           cleaned, stopped, effects, independent, badUnbound, badReplay>>

RecoverIO == /\ ioFailed /\ ioFailed' = FALSE
             /\ UNCHANGED <<owner, crashes, failures, intent, target, result, provenance,
                           cleaned, stopped, effects, independent, badUnbound, badReplay>>

Next == PublishIntent \/ GitEffect \/ Advance \/ PublishResult \/ IndependentWrite
        \/ Crash \/ Retry \/ Cleanup \/ Cancel \/ FailWrite \/ RecoverIO
Spec == Init /\ [][Next]_vars
FairSpec == Spec /\ WF_vars(PublishResult) /\ WF_vars(RecoverIO)

NoUnboundEffect == ~badUnbound
OneEffect == effects <= 1
RetainIndependent == ~badReplay
NoCanceledEffect == stopped => target = "base"
RecoveryProgress == (intent \in Candidates /\ target \in {intent, "later"})
                    ~> result = intent
=============================================================================
