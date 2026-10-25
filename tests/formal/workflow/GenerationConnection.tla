------------------------ MODULE GenerationConnection ------------------------
EXTENDS Naturals, Sequences
CONSTANT CheckCurrent
VARIABLES wf, generation, queue, pc, saved, accepted
vars == <<wf, generation, queue, pc, saved, accepted>>
fvars == <<generation, queue, pc, saved, accepted>>
W == INSTANCE Workflow WITH Works <- {"work"}, ReviewOnly <- {},
     MaxFailures <- 0, DeliverInputs <- TRUE, CheckEvidence <- TRUE,
     FenceResume <- CheckCurrent, GuardEffect <- TRUE, TruthfulObservation <- TRUE,
     GuardCleanup <- TRUE, GuardVersion <- TRUE, GuardBaseline <- TRUE,
     GuardLandingBaseline <- TRUE, CanonicalKinds <- {"pass"},
     CorruptPrewrite <- FALSE, TargetKinds <- {"unchanged"},
     GuardReviewRoute <- TRUE, LandingKinds <- {"pass"}, s <- wf

F == INSTANCE GenerationFence WITH CheckGeneration <- CheckCurrent
Init == /\ W!Init /\ F!Init
Prepare == /\ (W!Intake("work") \/ W!AttemptCreate("work") \/ W!Authorize("work"))
           /\ UNCHANGED fvars
Stop == /\ wf["work"].phase = "authorized" /\ ~wf["work"].canceled
        /\ queue = "Claimed" /\ pc["starter"] = "Read" /\ pc["settler"] = "Read"
        /\ W!Cancel("work") /\ UNCHANGED fvars
Read(a) == /\ wf["work"].canceled /\ F!Read(a)
           /\ IF a = "settler" /\ generation = 2
                  THEN W!GrantResumeAuthority("work") ELSE UNCHANGED wf
Replace == /\ F!Commit("starter") /\ F!Eligible("starter")
           /\ W!ReplaceGeneration("work")
Resume == /\ F!Commit("settler")
          /\ IF F!Eligible("settler") THEN W!Resume("work") ELSE UNCHANGED wf
Next == Prepare \/ Stop \/ Replace \/ Resume \/ (\E a \in F!Actors: Read(a))
Spec == Init /\ [][Next]_vars

\* Queue CAS revisions and cancellation epochs have different persisted
\* representations. Relate their ordinal epochs only until settlement;
\* the final queue write increments its revision without replacing authority.
MappedEpoch == queue # "Done" => generation = wf["work"].generation
CurrentAuthority == pc["settler"] = "Commit" =>
    saved["settler"] = wf["work"].authority
NoStaleResume == /\ F!NoStaleCommit /\ W!ResumeSafe
=============================================================================
