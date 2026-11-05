# feiyi web app: redesign spec

Scope: `web/app/src` (Paseo v0.12.0 fork, Expo + react-native-web + unistyles). Goal: more modern, responsive, accessible, on phones (360-430px) and desktop (>=1024px). Approach: retune the existing tokens and styles. No new components, no new token keys that other chunks depend on, no key renames (so call sites in ~185 files keep working).

All paths below are relative to `web/app/` unless stated. Line numbers are from the vendored copy at the time of writing.

Not verified: nothing here was run or rendered (no build, browser or device on the 1 GB VM). Contrast ratios were computed by hand from the hex values; QA should re-check with a tool.

## 1. What is wrong today (observed)

| Area | Finding | Where |
|---|---|---|
| Look | Accent is green `#20744A`; brand is orange-red `#E5553B`. `success` is an alias of `accent`, so changing accent would turn every success state orange. | `src/styles/theme.ts:288`, `:419` |
| Look | Muted text fails AA on hover/selected surfaces: `foregroundMuted #A1A5A4` on dark `surface3 #434645` = 3.8:1. `foregroundExtraMuted #717574` on `surface0` = 3.7:1. Light `foregroundExtraMuted #a1a1aa` on white = 2.5:1. | `theme.ts:460-477`, `:323-345` |
| Look | Light theme is cool zinc, dark theme is green-tinted charcoal. They do not look like one product. | `theme.ts:323-345` vs `:460-477` |
| Look | Nine font sizes (12-26) with `sm` = `code` = 12; body 15px; chat line-height 1.4. Web stack is whatever `system-ui` resolves to per OS (Roboto/Helvetica fallbacks). | `theme.ts:577-587`, `:635-645`; `src/components/message.tsx:322-325` |
| Look | Radii are small and tight (2/4/6/8/12/16); bubbles and cards read as generic. | `theme.ts:609-618` |
| Look | Hard-coded palette colours bypass tokens: cancel `palette.red[600]`, send error `red[500]`, realtime voice `green[600]`. | `src/composer/index.tsx:2574`, `:2602`, `:2663` |
| Look | `destructive #c64f43` (hue ~6) sits next to the orange accent (hue ~9). They would read as the same colour. | `theme.ts:474` |
| Phone | Composer icon buttons are 28x28 with no `hitSlop` (send, attach, voice, cancel, context meter). Pills above the composer are 32 high. | `src/composer/input/input.tsx:2318-2349`; `composer/index.tsx:2570-2597`; `composer/pill-styles.ts:8` |
| Phone | Tool-call rows are about 30px tall (22px icon + 2x4 padding) with a 14px label. | `message.tsx:878-907` |
| Phone | Composer text is 15px. iOS zooms the page on focus under 16px, which is why zoom is locked with `maximum-scale=1, user-scalable=no`. That fails WCAG 1.4.4 (resize text). | `input.tsx:2275`; `src/hooks/use-compact-web-viewport-zoom-lock.ts:4-5` |
| Phone | No `env(safe-area-inset-bottom)` anywhere on web. Composer bottom padding is a flat 16 under `viewport-fit=cover`, so it can sit under the home indicator in the installed PWA. Keyboard shift is handled only for fullscreen mode on web (visualViewport in `composer/dock/internal/fullscreen.tsx:48-58`). | `composer/index.tsx:2541`; `public/index.html:13` |
| Phone | Manifest locks `orientation: "portrait"`; breaks landscape phones and tablets. | `public/manifest.json:9` |
| Phone | Compact = `xs`/`sm` only (<720). A 768px portrait tablet gets the desktop shell: 320px sidebar + 448px chat. | `src/styles/unistyles.ts:6-12`; `src/constants/layout.ts:36-39`; `src/stores/panel-store/state.ts:24` |
| Chat | Chat column max 820px at 15px is about 95 characters per line. | `theme.ts:648`; `src/agent-stream/view.tsx:1621`, `:1642` |
| Chat | User bubble is `surface3 #434645`, the lightest grey surface in the dark theme; heavy against the thin assistant text. Timestamp is 13px, a size used nowhere else. | `message.tsx:311-319`, `:163` |
| Chat | Shimmer animation has no reduced-motion fallback. Only `chat-outline/rail.web.tsx` and `synced-loader.tsx` mention reduced motion. | `message.tsx:148-157` |
| Focus | One global ring, hard-coded `#20744a` (green, 2.6:1 against dark `surface0`, fails 3:1). | `public/index.html:88-91` |
| Nav | Phone sidebar is already 44px rows (good, `density.rowHeight`). Gaps: footer icon buttons use `density.iconButton` (44) but nav rows use `tightRowHeight` 36 on phone, below target. Sidebar default 320. | `src/styles/density.ts:22-31`; `src/components/left-sidebar.tsx:868-874`, `:941-948`; `sidebar/sidebar-workspace-row.tsx:489` |
| Nav | Compact header title is 14/16px with a 56px row; skeleton and metadata text use 12px. | `src/components/headers/compact-header.tsx:139`, `:154-166` |
| Pairing page | Own palette (blue accent `#6aa6ff`, `#0f1115` ground), 14px body, 8px-padded buttons (about 36px tall), no focus style. | `web/index.html:10-19` |

## 2. Tokens

Keys stay; values change. Single owner: `src/styles/theme.ts` (chunk A).

### 2.1 Palette

Accent usage rule: accent is spent on the primary action (send, `Button variant="default"`, switches/selection) and the focus ring. Nothing else.

Dark theme (`paseoDarkColors`, `theme.ts:460-477`; the other dark variants and their `accent` stay as they are):

| Token | Old | New | Note |
|---|---|---|---|
| surface0 | #181B1A | #181B1A | = logo ground |
| surface1 | #1E2120 | #1E2120 | |
| surface2 | #272A29 | #262A28 | |
| surface3 | #434645 | #343837 | muted text on it 3.8 -> 4.8:1 |
| surface4 | #595B5B | #444846 | |
| surfaceSidebar | #141716 | #141716 | |
| foreground | #fafafa (builder default `:391`) | #F2F4F3 | |
| foregroundMuted | #A1A5A4 | #A1A5A4 | 5.8:1 on surface2 |
| foregroundExtraMuted | #717574 | #909493 | 5.2:1 surface0, 4.6:1 surface2 |
| border | #252B2A | #252B2A | |
| borderAccent | #2F3534 | #2F3534 | |
| accent | #20744A | #E5553B | 4.7:1 as fill vs surface0 (3:1 needed) |
| accentBright | #7ccba0 | #F0735B | accent used as text/icon: 5.0:1 on surface2 |
| accentForeground | #fff (`:415`) | #181B1A | charcoal on #E5553B = 4.7:1; white would be 3.7:1 |
| destructive | #c64f43 | #E5536B | crimson, separated from accent hue; 4.7:1 on surface0 |
| destructiveForeground | #fff (`:418`) | #181B1A | add optional `destructiveForeground` to `DarkThemeConfig`; other variants omit it and keep `#ffffff` |
| ring | #d4d4d8 (`:392`) | #F0735B | 6.0:1 on surface0 |
| success (new config field `success`) | alias of accent (`:419`) | #35C264 | = `statusDotSuccess` dark. Decouples success from accent. |

Light theme (`lightSemanticColors`, `theme.ts:323-345`):

| Token | Old | New | Note |
|---|---|---|---|
| surface0 | #ffffff | #FBFBFA | |
| surface1 | #fafafa | #F5F6F5 | |
| surface2 | #f4f4f5 | #EDEFEE | |
| surface3 | #e4e4e7 | #E0E3E2 | |
| surface4 | #d4d4d8 | #CDD1D0 | |
| surfaceSidebar | #f4f4f5 | #F1F3F2 | |
| surfaceDiffEmpty | #f6f6f6 | #F2F4F3 | |
| foreground | #1a1a1e | #181B1A | brand charcoal |
| foregroundMuted | #71717a | #4F5453 | 6.7:1 on surface2 |
| foregroundExtraMuted | #a1a1aa | #646968 | 4.9:1 on surface2 |
| border | #e4e4e7 | #DDE0DF | |
| borderAccent | #ececf1 | #E8EAE9 | |
| accent | #20744A | #E5553B | fill only; text on it is charcoal |
| accentBright | #239956 | #B93A22 | accent as text/icon on light: 5.7:1 on white-ish |
| accentForeground | #ffffff | #181B1A | |
| destructive | #b04138 | #B3263F | 6.4:1 on white |
| primary / ring | #18181b | primary #181B1A, ring #B93A22 | |
| success (new config field) | alias of accent | #1F7A3E | >=4.5:1 on surface0 |

Status colours (`statusSuccess/Danger/Warning/Merged`, `statusDot*`, `theme.ts:140-218`) stay: they are already generated to a documented contrast rule.

Contrast check list for QA (AA, text 4.5:1, icons/borders/fills 3:1): foreground, foregroundMuted, foregroundExtraMuted on surface0/1/2/3 and sidebar; accentForeground on accent; accentBright on surface0-2; destructiveForeground on destructive; ring on surface0/sidebar. In both themes.

### 2.2 Type

Five sizes, same keys, so no call-site edits (`theme.ts:577-587`):

| Key(s) | Old | New | Use |
|---|---|---|---|
| sm, code | 12, 12 | 12 | metadata, timestamps, code in tool output and diffs |
| base | 14 | 14 | UI chrome: labels, rows, menus, buttons, pills |
| content, lg | 15, 16 | 16 | chat prose, composer input, form inputs, headers |
| xl, 2xl | 18, 20 | 20 | section/screen titles |
| 3xl, 4xl | 22, 26 | 26 | welcome/empty-state titles |

Also: `STREAM_METADATA_FONT_SIZE` 13 -> 12 (`message.tsx:163`). Line height: prose 1.5 (change the 1.4 multipliers at `message.tsx:325`, `input.tsx:2283`, `src/components/markdown/styles.ts:18`; `tool-call-details.tsx:855` is already 1.5). UI chrome 1.4 (unchanged). Compute inline as `Math.round(fontSize * 1.5)` like today; no new `LINE_HEIGHT` keys.

Weights: normal and medium for UI, semibold for titles only; no bold in chrome. Tabular figures for elapsed time, token counts and diff stats: add `fontVariant: ["tabular-nums"]` to `LiveElapsed` (`message.tsx:684`), `diff-stat.tsx`, and `usage/*` number text.

Font family (`theme.ts:635-645`, web only; native stays `system-ui`/`normal`):
- UI: one self-hosted webfont, **Geist** variable (SIL OFL), latin subset woff2, at `public/fonts/Geist-Variable-latin.woff2`, `@font-face { font-display: swap; font-weight: 100 900 }` in `public/index.html`, with `<link rel="preload" as="font" type="font/woff2" crossorigin>`. Stack: `"Geist", system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif`. Why: a single consistent face across Android/Linux/Windows/iOS instead of five different fallbacks; static host, no runtime dependency. Budget: if the latin subset is over 70 KB, drop the webfont and keep the current stack (everything else in this spec stands). Add the file to the service worker precache (the worker was added in commit 6d2346b; find it with `grep -ri precache` in the app build config, it was not in `src/`).
- Mono: no webfont. `ui-monospace, "SF Mono", "Cascadia Mono", Menlo, Consolas, "Liberation Mono", monospace`.
- User font override in Appearance settings keeps working (`appearance-style-boundary.tsx:25-35` patches the same keys); the default stack is only the fallback.

### 2.3 Radii (`theme.ts:609-618`)

| Key | Old | New |
|---|---|---|
| sm | 2 | 4 |
| base | 4 | 6 |
| md | 6 | 8 |
| lg | 8 | 10 |
| xl | 12 | 14 |
| 2xl | 16 | 20 |
| full | 9999 | 9999 |

Composer, user bubble, sheets, pills use `2xl` (20). Rows, tool-call cards, inputs use `lg` (10). Update the comment at `composer/pill-styles.ts:34` (16px tangent point -> 20px) and keep the 32px pill floor? No: pill floor becomes 36 on compact (see 4.2), so the 20px radius stays unclamped.

### 2.4 Spacing, geometry, breakpoints

- `SPACING` (`theme.ts:560-575`): unchanged (4-based). Rule: space between messages (16) > space inside a message (8-12); outer gutter on phone 16, on desktop 24.
- `DEFAULT_CONTENT_MAX_WIDTH` 820 -> 680 (`theme.ts:648`): about 80 characters of 16px prose. Wide code blocks and diffs scroll horizontally inside the column (already the case). Users with a saved width keep it.
- `density` (`styles/density.ts:22-31`): `tightRowHeight` compact 36 -> 44. Add `composerButton: byLayout(36, 28)` (compact 36 visual + `hitSlop` 4 = 44). `blockGap` unchanged.
- Breakpoints (`styles/unistyles.ts:6-12`): `md` 720 -> 900, `lg` 992 -> 1024. Compact now covers phones and portrait tablets; the desktop shell starts at 900. `DEFAULT_SIDEBAR_WIDTH` 320 -> 280 (`stores/panel-store/state.ts:24`).
- Focus ring: 2px, offset 2, colour `colors.ring`. Global fallback in `public/index.html:88-91` reads `var(--focus-ring, #F0735B)`; `appearance-style-boundary.tsx` writes `--focus-ring` on `document.documentElement` from `theme.colors.ring` on theme change. Control-level ring in `ui/control-geometry.ts:35-36` already exists and uses the ring token.
- Motion: durations 120-200ms, `transform`/`opacity` only. Reduced motion: in `public/index.html` add `@media (prefers-reduced-motion: reduce) { *, *::before, *::after { animation-duration: .01ms !important; animation-iteration-count: 1 !important; transition-duration: .01ms !important; scroll-behavior: auto !important } }`; in `message.tsx` `ensureWebToolCallShimmerKeyframes` (`:189`) and `NativeExpandableBadgeShimmer` (`:1019`) skip the shimmer when `matchMedia("(prefers-reduced-motion: reduce)").matches` / `AccessibilityInfo.isReduceMotionEnabled()` and show the loading label at `opacity: 0.72` (the existing `labelLoading` style, `:917`).

## 3. Chunks (parallel, disjoint file sets)

Each chunk touches only the files listed. No chunk reads a new token key from another chunk, so they merge in any order. One ordering rule is flagged in D.

### Chunk A. Tokens, shell, PWA (owner: design-system engineer)

Files: `src/styles/theme.ts`, `src/styles/density.ts`, `src/styles/unistyles.ts`, `src/constants/layout.ts` (only if a constant needs the new `md`), `src/stores/panel-store/state.ts`, `src/components/appearance-style-boundary.tsx`, `public/index.html`, `public/manifest.json`, `public/fonts/*` (new).

Changes: sections 2.1-2.4; `success` config field replaces `success: tint.accent` in both builders (`theme.ts:288`, `:419`) and `successForeground` uses surface0 (light) / `#181B1A` (dark); `--focus-ring` var; reduced-motion CSS; `@font-face`; `manifest.json`: `orientation` -> `"any"`, `name`/`short_name`/`description` "feiyi", and `apple-mobile-web-app-title` -> "feiyi" (`public/index.html:9`); `theme-color` stays `#181B1A`; pre-React background flash: add `@media (prefers-color-scheme: light) { html, body { background: #FBFBFA } }` next to `:36-41`.

Acceptance:
- Dark and light: every pair in the contrast list in 2.1 meets AA (QA with a contrast tool, record values).
- Switching the theme preference (Settings > Appearance, `THEME_OPTIONS` `theme.ts:781`) changes `--focus-ring` live; Tab ring is visible on a button, input, row and menu item in both themes.
- Success check marks and "connected/success" badges stay green; only primary actions, switches and the ring are orange.
- Only five distinct values exist in `FONT_SIZE`; `grep` of the built CSS/styles shows no 13px or 15px text.
- Window at 899px shows the phone layout; at 900px the desktop shell with a 280px sidebar and a chat column of at least 400px.
- App installs and rotates freely on Android/iOS (no portrait lock). `prefers-reduced-motion: reduce` leaves no running animation.
- Font loads with `swap` (no invisible text), works offline after first load, file under 70 KB or the fallback is used.

### Chunk B. Chat stream and tool output (owner: chat engineer)

Files: `src/components/message.tsx`, `src/components/markdown/styles.ts`, `src/components/tool-call-details.tsx`, `src/agent-stream/view.tsx`, `src/components/plan-card.tsx`, `src/components/archived-agent-callout.tsx`.

Changes:
- User bubble (`message.tsx:311-319`): `surface3` -> `surface2`, padding 12 vertical / 16 horizontal, `2xl` radius with the existing small top-right corner, max width 85% of the column on desktop, 92% on phone. Text 16/24.
- Assistant text stays unboxed (`:724-777`), 16/24; paragraph gap from `density.blockGap`.
- Message rhythm: gap between a user turn and the next assistant turn 16 (`containerFirstInGroup/LastInGroup`, `:305-310`); within a tool sequence 4 (`containerSpacing`, `:302`, `:872`); end of a sequence 16 (`:875`).
- Tool-call row (`expandableBadgeStylesheet`, `:868-907`): `pressable` gets `minHeight: { xs: 44, md: 32 }`, `paddingVertical` 4 (centered), radius `lg`; label 14 `foregroundMuted`, active/running `foreground`; secondary label (path/command) 12 mono `foregroundExtraMuted`, single line, truncated. Open-file button (`:943`) `hitSlop` 8 on desktop and 44x44 hit area on compact. Status is not colour alone: running = spinner/shimmer + label, failed = destructive icon + "failed" text.
- Expanded detail (`tool-call-details.tsx:820-1023`): container `surface1`, 1px `border`, radius `lg`; output/code 12 mono at 18px line height (`LINE_HEIGHT.diff` stays 22 for diffs); horizontal scroll inside, never page overflow; max height 320 on phone with an internal scroll and a visible "Show more" row at least 44 high; copy button 44x44 hit area on compact.
- Metadata row (timestamps, elapsed, copy, fork): 12px `foregroundExtraMuted`, always visible on touch (already: `message.tsx:417`), hover-revealed on desktop, tabular numbers. Copy/fork buttons 44x44 hit area on compact (`copyButton`, `:343-349`).
- Column: `view.tsx:1625-1645` gutters 16 phone / 24 desktop (`paddingHorizontal: { xs: 4, md: 6 }`, replacing 3/4 plus the extra `theme.spacing[2]` at `:1623`, `:1644`).
- Empty and error: `view.tsx:1646-1651` empty state shows the agent name, status and one next action ("Send a message to start"); loading uses skeletons of message height; load-error row has a "Retry" button >=44 high with text naming the cause.
- Reduced motion per 2.4.

Acceptance:
- 390px: no horizontal page scroll with a 200-character unbroken string, a wide table and a 120-column code block (only the block scrolls). 1280px: prose lines 70-85 characters.
- Every tappable row (tool call, expand, copy, open file, fork) has a hit area >=44x44 at 390px (QA measures in devtools at 390 wide); >=32 on desktop.
- Body text 16px/24px; metadata 12px; contrast per 2.1 in both themes, including on the user bubble and expanded tool detail.
- Tool states present and distinguishable without colour: pending, running, completed, failed, canceled.
- With reduced motion on, no shimmer movement; running tools still show a label change.
- Keyboard: Tab reaches each tool row, Enter/Space expands, visible ring.

### Chunk C. Composer (owner: composer engineer)

Files: `src/composer/index.tsx`, `src/composer/input/input.tsx`, `src/composer/input/presentation.tsx`, `src/composer/pill-styles.ts`, `src/composer/agent-controls/*`, `src/composer/dock/index.tsx`, `src/composer/dock/internal/*`.

Changes:
- Wrapper (`input.tsx:2203-2231`): radius `2xl`; border `borderAccent`, focus-within border `ring` plus a 1px inner `ring` at 40% alpha (replaces the border-colour-only transition, `:2226-2229`, so focus is >=3:1 and not colour alone... the 2px outline is added via `outlineWidth` on web); background `surface1`; padding compact 12/12, desktop 16/12 (`:2211-2223`); `inputWrapperCompact` shadow (`:2233`) kept (this is the single elevated element on the screen).
- Input text (`:2270-2289`): 16px, line height 24 on web, caret colour `accentBright`. No further zoom risk on iOS.
- Buttons (`:2318-2349`, `composer/index.tsx:2570-2597`): `width/height: { xs: 36, md: 28 }` and `hitSlop={4}` on compact so the hit area is 44. Send button: accent fill, `accentForeground` arrow, disabled = `surface3` fill with `foregroundExtraMuted` arrow (not just 0.5 opacity), loading = spinner in place of the arrow, `aria-label` "Send". Cancel button: `colors.destructive` fill (replace `palette.red[600]`, `:2574`). Voice recording: `colors.destructive` (already) + pulsing is removed. Realtime-voice active: `colors.statusSuccess` fill (replace `palette.green[600/800]`, `:2602-2603`). Send error text `colors.destructive`, 14px, with an icon (replace `palette.red[500]`, `:2663`).
- Pills above the composer (`pill-styles.ts:8`, `:26-51`): `COMPOSER_PILL_MIN_HEIGHT` 32 -> 36 on compact (44 hit area via `hitSlop`), radius `2xl`; label 12 -> 14 on compact. Update `resolveComposer*Clearance` arithmetic to use the same constant (they already do).
- Mode/model controls (`agent-controls/control.tsx`, `mode-control.tsx`, `model-sheet.tsx`): compact trigger rows 44 high; model picker already opens as a bottom sheet on compact; keep.
- Safe area and keyboard (web PWA): `inputAreaContainer.paddingBottom` (`index.tsx:2541`) -> `paddingBottom: "max(16px, env(safe-area-inset-bottom))"` on web (RN Web passes the string through); dock `content`/`composer` unchanged. On iOS the visual viewport shrinks on keyboard open: the composer must stay fully visible above the keyboard in inline mode, not only fullscreen. Fix: in `dock/index.tsx` add the same `visualViewport` listener used in `internal/fullscreen.tsx:48-58` and apply `paddingBottom = layoutViewportHeight - visualViewport.height - visualViewport.offsetTop` (>=0) to `surface` when compact and focused. If device testing shows the browser already resizes the layout viewport (Chrome Android), the computed value is 0 and nothing changes.
- States to verify: default, hover (`iconButtonHovered` surface2), focus-visible ring on every button, pressed, disabled, recording, sending, queued (queue rows `index.tsx:2629-2658`: action buttons 32 -> 44 hit area), read-only (dotted border, `:2238`), attachment error.

Acceptance:
- 390px: all composer controls have a 44x44 hit area; composer text is 16px; focusing the input does not zoom the page (iOS Safari, with the zoom lock removed in chunk D).
- Installed PWA on a notch/home-indicator iPhone: composer bottom edge clears the home indicator by >=0 and the keyboard by 0 gap/overlap; typing 6 lines grows the input to the existing cap without covering the header.
- Tab order: input -> attach -> mode/model -> voice -> send; ring visible on each in both themes; Enter sends, Shift+Enter newline (unchanged).
- No `palette.*` references remain in `src/composer/**` (grep).
- Disabled send is distinguishable by colour and by `aria-disabled`/label, not opacity alone.

### Chunk D. Navigation, headers, menus, sheets (owner: navigation engineer)

Files: `src/components/left-sidebar.tsx`, `src/components/sidebar/*`, `src/components/sidebar-workspace-list.tsx`, `src/components/sidebar-callout.tsx`, `src/components/headers/*`, `src/components/ui/menu/*`, `src/components/ui/form-field.tsx`, `src/components/ui/search-field.tsx`, `src/components/ui/combobox.tsx`, `src/components/adaptive-modal-sheet.tsx`, `src/hooks/use-compact-web-viewport-zoom-lock.ts`, `src/screens/workspace/workspace-desktop-tabs-row.tsx`.

Changes:
- Sidebar (`left-sidebar.tsx:867-957`): selected workspace row = `surfaceSidebarSelected` fill plus a 3px `accent` bar on the leading edge (selected is not colour-only: bar + `foreground` text + `aria-current="page"`); hover `surfaceSidebarHover`. Row height from `density.rowHeight` (44/36, `sidebar-workspace-row.tsx:489`); nav rows `tightRowHeight` now 44 on compact (chunk A value). Row title 14 `foreground`, meta line 12 `foregroundMuted`; status dot per `statusDot*` plus a text label in the row's accessible name ("Claude, running"). Section headers 12 `foregroundExtraMuted` normal case (no tracked uppercase).
- Phone drawer: width `min(86vw, 360px)`, scrim `rgba(0,0,0,0.5)`, slide 200ms `transform` only, swipe-to-close stays (`useCloseAgentListGesture`, `left-sidebar.tsx:551`); top/bottom padding = safe-area insets (already, `:562-569`); focus moves to the close button on open and returns to the opener on close; Escape closes on web.
- Desktop (>=900): persistent sidebar 280 default, resizable 200-600 (unchanged, `state.ts:25-26`); collapsible with the existing shortcut; footer icon buttons 28 (`density.iconButton`) get tooltips (exist).
- Switching agents/projects: the workspace list stays the single switcher; add nothing new. Long-press drag-reorder stays.
- Header (`compact-header.tsx:134-175`): height 56 + top safe area; title 16 medium single line, subtitle 12 `foregroundMuted`; back/menu buttons 44x44 (`HeaderToggleButton`, check `headers/header-toggle-button.tsx`); the header sits on `surface0` with a 1px `border` bottom only once the list is scrolled (no permanent shadow).
- Menus (`ui/menu/menu-item.tsx:444`): compact item `minHeight` 44, desktop unchanged (`MENU_ITEM_HEIGHT`); item 14, shortcut 12 `foregroundExtraMuted`; destructive items use `destructive` text + icon; popover surface `surface2`, 1px `borderAccent`, `shadow.md`, radius `xl`. Menus open as bottom sheets on compact (existing `adaptive-modal-sheet`), sheet top radius `2xl` (`adaptive-modal-sheet.tsx:234`), max height 85% (`:91`), bottom padding = safe-area inset, handle 4x36 centered.
- Form inputs on compact (`form-field.tsx`, `search-field.tsx`, `combobox.tsx`): font 16 (`{ xs: lg, md: base }`), height `CONTROL_HEIGHTS.field` 44 (already `control-geometry.ts:27`).
- Zoom lock: after the input change above, delete `maximum-scale=1, user-scalable=no` from `COMPACT_WEB_VIEWPORT_CONTENT` (`use-compact-web-viewport-zoom-lock.ts:4-5`) so it equals the default content. Ship in the same PR as the form-input font change, and after chunk C, otherwise iOS zooms on focus.
- Empty/error/loading: sidebar skeleton rows sized 44 (exists, `SidebarAgentListSkeleton`); empty project list: "No projects yet" + "Open project" button (`sidebar/empty-states.tsx`); host unreachable: row text "Can't reach <host>" + "Retry" 44 high.

Acceptance:
- 390px: open drawer with the header button and with an edge swipe; focus lands inside; Escape and scrim tap close; background does not scroll; selected row identifiable in greyscale.
- All sidebar rows, header buttons, menu items, sheet actions >=44 high at 390px; desktop rows 36, icon buttons 28 with tooltips.
- Pinch zoom works on the phone layout (viewport meta has no `maximum-scale`/`user-scalable=no`) and focusing any input does not zoom the page.
- Keyboard (desktop): Tab through nav rows, Enter opens, arrow keys in menus, Escape closes menus; ring visible in both themes.
- 899px shows the drawer; 900-1023px shows the desktop shell with a 280px sidebar and chat >=400px.

### Chunk E. Accent sweep and pairing page (owner: any engineer; light work)

Files: `web/index.html` (the daemon pairing page, repo path `web/index.html`, not under `web/app`), plus these files for the accent/success sweep: `src/components/question-form-card.tsx`, `realtime-voice-overlay.tsx`, `dictation-controls.tsx`, `welcome-screen.tsx`, `src/panels/setup-panel.tsx`, `src/git/commits-section/commit-graph-node.tsx`, `src/desktop/browser/pane/index.electron.tsx`, `src/components/split-drop-zone.tsx`, `src/components/resize-handle.tsx`. (90 uses of `colors.accent|accentBright|success|palette.green/red` in 47 files; only the files above have 3+ uses. Others are single uses to be spot-checked.)

Changes:
- Sweep: where `colors.accent` means "success/ok/done" use `colors.success` (now green, chunk A); where it means "selected/active/primary" keep `accent`; where `accentBright` is used as text keep (now `#F0735B`/`#B93A22`). Replace `palette.green/red` with `statusSuccess` / `destructive`.
- Pairing page `web/index.html:10-19`: ground `#181B1A`, panel `#1E2120`, line `#2F3534`, fg `#F2F4F3`, mute `#A1A5A4`, accent `#E5553B` with button text `#181B1A`, bad `#E5536B`; add `@media (prefers-color-scheme: light)` using the light values from 2.1; body 16px; buttons and input min-height 44, radius 10, padding 10px 14px; `:focus-visible { outline: 2px solid var(--ring); outline-offset: 2px }` (`--ring`: `#F0735B` dark, `#B93A22` light); `viewport-fit=cover` and `padding: max(16px, env(safe-area-inset-*))`; reduced-motion block from 2.4; "Connecting..." state gets `role="status"`; `h1` shows the logo mark inline (inline SVG from `web/logo.svg`, `alt`-less because the heading text carries the name, `aria-hidden="true"`). Copy: replace "Scan with the Paseo app" with "Scan with the feiyi app".

Acceptance:
- Pairing page: all five views (checking, unreachable, noPassword, login, pairing) pass AA in both colour schemes; every button >=44px; Tab order login: password -> Unlock; ring visible; error text linked with `aria-describedby` on the input (`loginErr`, `pairErr`).
- Sweep: with the app in dark and light, no green appears on a primary action and no orange appears on a success/connected state (visual pass over question form, voice overlay, dictation, welcome, setup panel, commit graph, drop zones).

## 4. Responsive behaviour summary

| | Phone (360-430, and <900) | Desktop (>=900; target >=1024) |
|---|---|---|
| Shell | Single column; sidebar is an overlay drawer min(86vw, 360); 56px header + top inset | Sidebar 280 (200-600, resizable) + chat; tabs row 36px |
| Chat column | Full width, 16px gutters, 16/24 text | Max 680 centered, 24px gutters |
| Hit targets | >=44 (hit area; visual may be 36) | 28-36 visual, hover and tooltips |
| Composer | Bottom, safe-area padding, 36px buttons +hitSlop, 16px text | Max 680 centered, 28px buttons |
| Menus | Bottom sheets, 44px items | Popovers, 36px items |
| Zoom | Allowed | Allowed |

## 5. Deliberately unchanged

- Component structure, routing, stores, gestures (drawer swipe, drag-reorder, long-press), Gorhom sheets, command center, keyboard shortcut system.
- `SPACING` scale, `ICON_SIZE` (compact icon sizes already come from `density.ts:15`), `BORDER_WIDTH`, shadows (`theme.ts:680-699`), the `CONTROL_HEIGHTS` ladder (`control-geometry.ts:45`).
- Status colours and status-dot generation rules; diff and syntax colours; terminal ANSI colours.
- Other dark variants (Zinc, Midnight, Claude, Ghostty, Pure Black): not retuned; they gain the new type, radii and layout but keep their own accent. Only the default feiyi dark and the light theme change colour.
- Native iOS/Android code paths (`*.native.tsx`): token changes apply, no layout edits.
- Service worker logic, daemon protocol, desktop (Electron) window chrome constants.
- Icon set (lucide) and stroke width 1.5.

## 6. Risks

- Retuning `FONT_SIZE` also changes Appearance-settings scaling: confirm `appearance-style-boundary.tsx` and `screens/settings/appearance/*` still offer sensible user overrides (they patch the same keys at runtime).
- Breakpoint `md` 720 -> 900 moves every `{ xs, md }` style value switch and `isCompactBreakpoint` for 720-899px windows; QA should sweep Settings (`SETTINGS_DESKTOP_SPLIT_MIN_WIDTH` 720, `layout.ts:20`), which needs 720px for list+detail and now falls into compact first. Either keep Settings split at its own width check (already independent) or accept the drawer layout there.
- iOS keyboard handling in the web PWA (chunk C) needs a real device; not verifiable from the code alone.
- Geist file size and licence file placement are unchecked.
- Contrast numbers are hand-computed from hex; they were not measured with a tool.
