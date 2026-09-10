# Shared Context team policy

This file is delivered to the model at run time by `sctx`, one section per channel. Only the four
`##` headings below are read; this paragraph and any other prose are ignored. Edit a section to
change what every Session in this installation is told. Delete a section to say nothing there.

## session

Stored text is Chinese, with identifiers, paths, commands, and error codes in their original spelling, and every version, commit, and date written absolutely.
Record one `progress` summary per task boundary, never per turn.

## checkpoint

Also worth keeping: a correction the user made to your own proposal that later proved right, and, as `context_kind: progress`, a stage summary after a long autonomous stretch. Not worth keeping: anything the code, `git log`, or a PR already states plainly; anything an accepted Context in the Pack already says, unless you submit only the delta; a routine build, compile, or test success. Record a `progress` row only at a task boundary, at most one per boundary and never per turn or per file: which paths changed for what purpose, what state things are in now, what was verified and what was not, and what remains.
When the user asks you to record something, apply the same filter: if the point is derivable from the code or history, record instead the one part that is not.
A Checkpoint is usually one to three Claims; needing more than five in one call means the filter did not run. When unsure, leave it out.

## stop

A `progress` row is finished only when the next person could resume from this branch in five minutes with it alone.

## triage

Discard grounds: an exact_duplicate of a still-accepted Context adding no applicability condition or Evidence, or process-level code reading that is not a progress summary. Confirm only rows carrying a genuine decision, contract, verified conclusion, counter-intuitive finding, newly understood mechanism, a user correction to your proposal that later proved right, or a progress summary of paths changed, verified state and remaining work.
