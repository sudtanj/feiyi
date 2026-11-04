---
name: anti-slop-code
description: "Use before writing, editing or reviewing code and code comments. Keeps changes minimal, idiomatic to the surrounding codebase, and free of AI tells such as narrating comments, needless wrappers and invented APIs."
---
<!-- Adapted from miqdadbadjuber/anti-slop (MIT). -->

# Anti-slop code

Code slop is code that looks thorough but adds no value: comments that restate the line, guards for cases that cannot happen, abstractions with one caller, stubs that pretend to work. The goal is the smallest change that solves the task and reads like the rest of the repo.

## Read first

1. Open the files around your change. Note naming, error handling, imports, formatting, test style and how similar problems are already solved.
2. Search for an existing helper before writing a new one.
3. Check that every function, flag, config key, package and method you call exists in this repo or in the installed version of the dependency. If you cannot confirm it, do not call it.

## Comments

- Delete comments that restate the next line, the signature or the class name.
- Delete narration such as "Step 1", "First", "Next", "Finally". If the flow is hard to follow, restructure the code.
- Delete decorative banners, ALL CAPS labels, end-of-block markers (`} // end if`) and emoji.
- Keep comments that explain why: a business rule, a workaround, a security or concurrency constraint, a protocol quirk, a performance trade-off, an API contract.
- Keep each comment to one line, two at most. One comment per logical block, not per line.
- A TODO must name a concrete task and enough context to act on. "TODO: improve this" is deleted.
- Do not leave comments that describe the edit itself ("added", "changed", "new", "fixed bug").
- Doc blocks that only repeat parameter names and types are noise. Document behavior, edge cases and side effects.

## Defensive code

- Validate at system boundaries: user input, network, files, external APIs. Do not re-validate values your own code already guarantees.
- No try/catch that swallows errors, logs and continues, or returns a fake default. Handle the error meaningfully or let it propagate.
- No null checks, fallbacks or optional chaining for values the types or callers rule out.
- No feature flags, compatibility shims or config options nobody asked for.

## Abstractions

- No wrapper that only forwards to another function.
- No interface, factory, base class or generic with a single implementation or caller.
- Do not extract a helper for three similar lines used once. Duplicate until a real pattern shows.
- No new utility file when a function in an existing module fits.
- Do not refactor, rename or reformat code outside the task. Keep the diff reviewable.

## Stubs and placeholders

- No `TODO`, `pass`, `throw new Error("not implemented")`, empty handlers or mocked return values presented as finished work.
- If you cannot finish a part, say so in your report instead of hiding it in the code.
- No hardcoded sample data, fake IDs or example credentials in real code paths. Never write secrets.

## Names and consistency

- Names say what a thing is or does in this domain. Avoid `data`, `result`, `temp`, `helper`, `manager`, `handler`, `utils` unless the codebase already uses them that way.
- Match the project's casing, import order, error types, logging calls and test layout, even if you would choose differently.
- Use the language and framework idioms already in the repo. Do not introduce a new library for something the standard library or existing dependencies do.

## Tests

- Test behavior, not implementation details. Each test fails for a reason a reader can name.
- No tests that only assert a mock returns what you told it to.
- Follow the existing test framework and file layout. Do not claim tests pass unless you ran them.

## Honest reporting

State what you changed, what you checked, what you did not run and any assumption you made. Do not describe code as "robust", "production-ready" or "comprehensive".

## Before you finish

- [ ] Every API, option and import I used exists here (checked, not assumed).
- [ ] No comment restates code, narrates steps, or describes my edit.
- [ ] No guard, fallback or catch for a case that cannot occur.
- [ ] No wrapper, interface or helper with a single use.
- [ ] No TODO, stub, fake data or placeholder left in working code.
- [ ] Naming, formatting and error handling match neighboring files.
- [ ] The diff touches only what the task needs.
- [ ] My report says what I verified and what I did not.
