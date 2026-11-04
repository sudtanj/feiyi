---
name: systematic-debugging
description: "Use when a bug, test failure or unexpected behavior appears, before proposing a fix. Find the root cause first; change one thing at a time."
---
<!-- Adapted from obra/superpowers (MIT). -->

# Systematic debugging

Fixing symptoms creates new bugs. Find the cause, then fix it once.

## Steps

1. Reproduce. Get a reliable, minimal way to trigger the failure. Read the full error and stack trace. If you cannot reproduce it, gather more data; do not guess.
2. Check what changed: recent diffs, config, dependencies, environment.
3. Trace backward from the bad value or failing line to where it first went wrong. In multi-part systems, log or inspect at each boundary to see which part breaks.
4. Compare with similar code that works and list every difference.
5. State one hypothesis ("X fails because Y") and test it with the smallest change or probe.
6. Fix the root cause, one change at a time. Confirm the original failure is gone and nothing else broke, using the cheapest check available.

## Stop rules

- No fix before you can say the root cause.
- Do not stack several changes: you will not know which one worked.
- If a fix fails, return to step 3 with what you learned; do not add another patch on top.
- After 3 failed fixes, stop and question the design: the pattern, not the line, may be wrong. Say so in your report.

## Red flags

- "Quick fix for now", "just try this".
- Retrying without understanding why it failed.
- Fixing in a new place what broke in another.
