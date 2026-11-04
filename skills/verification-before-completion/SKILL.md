---
name: verification-before-completion
description: "Use before saying work is done, fixed, passing or ready, and before committing or reporting. Requires fresh evidence from a command you ran, using the cheapest check that proves the claim."
---
<!-- Adapted from obra/superpowers (MIT). -->

# Verification before completion

Claiming work is done without evidence is a guess. Run the check, read the output, then state the result.

## The rule

No "done", "fixed", "passing" or "ready" without fresh evidence from this session: a command you ran after the last edit, whose output you read.

## Check first

1. Name the claim ("the parser handles empty input", "the bug no longer reproduces").
2. Pick the cheapest command that proves exactly that claim: a syntax check, one focused test, a grep, running the one function or request.
3. On a low-resource host never run heavy full builds, whole test suites or servers to prove a small claim. If only a heavy check can prove it, say it is unverified and name the command for someone else to run.
4. Run it fresh. Earlier runs, other people's reports and "it should work" are not evidence.
5. Read the whole output: exit code, failures, warnings.

## Honest claims

- Tests pass only if you ran them and they passed. A linter passing says nothing about tests; a build passing says nothing about behavior.
- A fix is proven by the original failure no longer happening, not by the code looking right.
- A helper's or sub-agent's report of success is a claim: check the files or diff before repeating it.
- If a check fails, report the failure with its output. Do not report partial work as complete.

## Red flags

- Words like "should", "probably", "seems to".
- Satisfaction before verification ("Great, done!").
- Committing or reporting on the strength of the last run, before further edits.

## Before you finish

- [ ] I ran a command after my last edit that proves the claim.
- [ ] I read its full output, not just the last line.
- [ ] The check was the cheapest one that proves the claim, not a heavy build.
- [ ] My report lists what I verified and what I did not.
