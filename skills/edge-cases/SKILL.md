---
name: edge-cases
description: "Use before planning, implementing or reviewing any change. A checklist of the inputs, states, timing, resources, permissions and UI states that break software, each of which must be answered."
---

# Edge cases

Most production bugs sit in cases nobody wrote down. Go through every group below for the change in front of you.

## Inputs

- Empty, one item, the maximum, and one over the limit.
- Duplicates, and the same item arriving twice.
- Unicode: combining marks, right-to-left text, emoji, very long strings, embedded NUL or newlines.
- Malformed or truncated data, and the wrong type or a missing field.

## State

- First run with no data, and the same code on data written by an older version (migration).
- Restart or crash in the middle of an operation.
- Partial failure: step two of three fails. What is left behind?
- Retry: is running it twice safe (idempotent)?

## Time and concurrency

- Two callers at once, and the double submit.
- Timeouts on every external call, and what happens after one.
- Clock changes, time zones, daylight saving, expiry at the boundary.
- Ordering: events arriving late or out of order.

## Resources

- Disk full, and a write that fails halfway.
- Input far larger than the tests use.
- A dependency that is slow, down or returns garbage.
- Anything that grows without a bound: logs, caches, queues, history.
- Memory on the smallest machine this runs on.

## Permissions and trust boundaries

- Who can call this, and with which credentials?
- What must the caller not be able to see or change?
- Which values come from the client and are trusted by mistake?

## UI

- Empty, loading, error, offline and disabled states.
- Slow response, repeated taps, and leaving the screen mid-request.

## Rule

For every item write applies / N/A (why) / handled (test name or file:line). One line per group is enough; give detail only for items marked handled or blocker. An unanswered item is a blocker.
