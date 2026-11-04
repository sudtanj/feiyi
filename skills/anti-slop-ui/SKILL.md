---
name: anti-slop-ui
description: "Use before designing or building any web page, component or app screen. Avoids generic AI-looking UI and requires real states, accessibility, and a deliberate type scale and palette."
---
<!-- Adapted from miqdadbadjuber/anti-slop and Leonxlnx/taste-skill (MIT). -->

# Anti-slop UI

UI slop is whatever you picked because it is the common default, not because it fits this product. Decide the product's real content, audience and main user decision first, then design from that.

## Start from the product

1. Write down who uses this, the one decision or task the screen supports, and the real content (copy, data, images) available.
2. Look for an existing design system, `DESIGN.md`, tokens or components in the repo and use them. Do not invent a parallel style.
3. Build sections and layouts from that content. Drop any section that exists only because landing-page templates have it.

## Avoid these defaults

- Purple, indigo or blue-to-violet gradients, rainbow or neon gradients, gradient text, gradient buttons, gradient blobs.
- Emoji used as icons or decoration. Sparkle, robot and orb glyphs used as "AI" icons.
- The stock page: centered hero, three icon-topped cards, logo bar, testimonials, FAQ, closing CTA. Also bento grids and three-column pricing with a highlighted middle tier, unless the content truly calls for them.
- Fake stats ("10,000+ users", "99.9% uptime"), invented testimonials, made-up logos, fake activity feeds, trend deltas with no real comparison period. Use real data, a clearly labeled placeholder, or nothing.
- Pill badges or eyebrow labels above a headline that repeat it ("AI Powered", "New"). Pulsing status dots that mark no real state.
- Glass blur, glows, heavy shadows and large radii on everything. Use them on one or two elements at most, and pick one radius scale.
- Monospace or wide-tracked tiny uppercase labels as a generic "tech" look.
- Decorative grids, dot patterns and fake terminals, unless the product is a CLI.
- Endless pulse, bounce or float animations.

## Spacing and layout

- Use one spacing scale (for example multiples of 4 or 8px) and one grid with consistent outer margins. No arbitrary one-off values.
- Space between sections is larger than space inside them.
- Vary composition between neighboring sections. Do not repeat identical cards for everything.
- Left-align text by default. Center at most one moment.
- Design the phone layout as its own composition, not a squeezed desktop.
- App screens are built around the user's decision, not a default shell of sidebar, stat cards, chart and table. Choose table columns and charts for the question they answer.

## Type and color

- Define a type scale up front: at most five sizes and one or two families, chosen for brand fit. Body text 16px or larger, line height about 1.5, lines of 45 to 75 characters. Headlines stay within two lines at every width.
- Use tabular figures for numbers that align in columns.
- Define the palette up front: one ground, one ink, one accent, plus neutrals. Spend the accent on the primary action and one other place, not on every link and icon.
- Body text in full ink contrast. Grey is for secondary notes, and it must still pass contrast.
- Check light and dark themes if the product supports both, and pick the default from the audience.

## Real states

Every interactive element needs default, hover, focus, active and disabled states. Every data view needs:

- Loading: a skeleton or indicator sized like the final content.
- Empty: names why it is empty and offers the next action. "No data" is not enough.
- Error: names the cause and a way to retry or fix.
- Success and partial states where they can occur.

Every link, button, menu and form must work, or be removed, or be labeled "Coming soon". Use honest placeholders (`email@example.com`), not realistic fake people.

## Accessibility

- Use real `<a>`, `<button>`, `<label>`, `<input>` and heading elements. One `h1` per page.
- Visible focus indicator, full keyboard operation in reading order, no keyboard traps.
- Accessible name on every control. Icon-only buttons get `aria-label`. Every informative image has alt text, and decorative ones have empty alt.
- Text contrast at least 4.5:1 (3:1 for large text). Do not use color alone to signal state.
- Link field hints and errors to inputs with `aria-describedby`.
- Respect `prefers-reduced-motion`. Animate only `transform` and `opacity`, and only for a stated purpose.
- Content stays readable if scripts fail.

## Performance basics

Give images and video fixed dimensions or an aspect ratio to prevent layout shift. Do not lazy-load the hero image. Defer third-party scripts. Report only metrics you measured.

## Before you finish

- [ ] No purple default gradient, emoji icon, or stock hero-plus-three-cards layout.
- [ ] No invented stats, testimonials, logos or feed items.
- [ ] Spacing, radii, type sizes and colors come from a defined scale or palette.
- [ ] Loading, empty, error and disabled states exist and say what to do next.
- [ ] Every control works, is removed, or is labeled "Coming soon".
- [ ] Keyboard, focus, labels, alt text and contrast verified by inspection.
- [ ] Phone and desktop layouts both considered; reduced-motion fallback present.
- [ ] Anything I could not check or run is named in my report.
