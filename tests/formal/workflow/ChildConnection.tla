-------------------------- MODULE ChildConnection --------------------------
EXTENDS Naturals
CONSTANTS RequireReceipt, CheckGeneration, CheckOwner
VARIABLES wf, generation, phase, held, receipt, parentAlive, stopped,
          badExecution, badStop, badSignal, executions, receiptOwner, badOwnerSignal
cvars == <<generation, phase, held, receipt, parentAlive, stopped,
           badExecution, badStop, badSignal, executions>>
vars == <<wf, cvars, receiptOwner, badOwnerSignal>>
W == INSTANCE Workflow WITH Works <- {"work"}, ReviewOnly <- {},
     MaxFailures <- 2, DeliverInputs <- TRUE, CheckEvidence <- TRUE,
     FenceResume <- TRUE, GuardEffect <- TRUE, TruthfulObservation <- TRUE,
     GuardCleanup <- TRUE, GuardVersion <- TRUE, GuardBaseline <- TRUE,
     GuardLandingBaseline <- TRUE, CanonicalKinds <- {"pass"},
     CorruptPrewrite <- FALSE, TargetKinds <- {"unchanged"},
     GuardReviewRoute <- TRUE, LandingKinds <- {"pass"}, s <- wf

C == INSTANCE ChildLaunchIdentity WITH RequireReceipt <- RequireReceipt,
     CheckStop <- TRUE, CheckGeneration <- CheckGeneration
Init == /\ W!Init /\ C!Init /\ receiptOwner = <<"no-task">> /\ badOwnerSignal = FALSE
Prepare == /\ (W!Intake("work") \/ W!AttemptCreate("work") \/ W!Authorize("work")
                \/ W!ChooseForeground("work") \/ W!Dispatch("work"))
           /\ UNCHANGED <<cvars, receiptOwner, badOwnerSignal>>
Reserve == /\ wf["work"].phase = "dispatched" /\ ~wf["work"].canceled
           /\ wf["work"].generation = generation + 1
           /\ C!Reserve /\ UNCHANGED <<wf, receiptOwner, badOwnerSignal>>
Create == /\ C!Create /\ UNCHANGED <<wf, receiptOwner, badOwnerSignal>>
Receipt == /\ C!ChildReceipt /\ receiptOwner' = wf["work"].task
           /\ UNCHANGED <<wf, badOwnerSignal>>
Publish == /\ C!ParentPublish /\ UNCHANGED <<wf, receiptOwner, badOwnerSignal>>
Execute == /\ (RequireReceipt => receiptOwner = wf["work"].task)
           /\ C!Execute /\ W!Deliver("work")
           /\ UNCHANGED <<receiptOwner, badOwnerSignal>>
Crash == /\ C!Crash /\ UNCHANGED <<wf, receiptOwner, badOwnerSignal>>
Stop == /\ C!RequestStop /\ W!Cancel("work")
        /\ UNCHANGED <<receiptOwner, badOwnerSignal>>
Signal == /\ (CheckOwner => receiptOwner = wf["work"].task)
          /\ C!Signal /\ W!Crash("work")
          /\ badOwnerSignal' = (badOwnerSignal \/ receiptOwner # wf["work"].task)
          /\ UNCHANGED receiptOwner
Settle == /\ C!Settle /\ W!Writer("work")
          /\ UNCHANGED <<receiptOwner, badOwnerSignal>>
RecoverChild == /\ C!Recover /\ W!Crash("work")
                /\ UNCHANGED <<receiptOwner, badOwnerSignal>>
RecoverWork == /\ W!Recover("work")
               /\ UNCHANGED <<cvars, receiptOwner, badOwnerSignal>>
Replace == /\ ~held /\ W!ReplaceGeneration("work")
           /\ UNCHANGED <<cvars, receiptOwner, badOwnerSignal>>
Grant == /\ W!GrantResumeAuthority("work")
         /\ UNCHANGED <<cvars, receiptOwner, badOwnerSignal>>
Resume == /\ ~held /\ W!Resume("work")
          /\ UNCHANGED <<cvars, receiptOwner, badOwnerSignal>>
\* Untrusted persisted receipt identity must never authorize a signal.
WrongOwner == /\ phase = "receipted" /\ receiptOwner = wf["work"].task
              /\ receiptOwner' = <<"task", "other">>
              /\ UNCHANGED <<wf, cvars, badOwnerSignal>>
Next == Prepare \/ Reserve \/ Create \/ Receipt \/ Publish \/ Execute
        \/ Crash \/ Stop \/ Signal \/ Settle \/ RecoverChild \/ RecoverWork
        \/ Replace \/ Grant \/ Resume \/ WrongOwner
Spec == Init /\ [][Next]_vars
NoUnrecordedExecution == C!NoUnrecordedExecution
NoWrongSignal == C!NoWrongSignal
NoWrongOwnerSignal == ~badOwnerSignal
MappedTask == phase = "executing" =>
    /\ receiptOwner = wf["work"].task
    /\ generation = wf["work"].generation
    /\ wf["work"].worker = W!WorkerId("work", generation)
    /\ C!OwnedExecution
=============================================================================
