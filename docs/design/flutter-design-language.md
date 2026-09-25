# Adjutant — Flutter Design Language

**Status:** Draft for review
**Date:** September 2026
**Decides:** What Adjutant looks and feels like — the visual identity of the troop's administration tool.

---

## 1. Philosophy

Adjutant is not a corporate SaaS product. It is a sovereignty tool — built by scouts, for scouts, self-hosted, owned by the troop. The design language should reflect that.

**Four principles:**

1. **Clear, not clever.** Every screen should answer the question "what do I do next?" without requiring a tutorial. Scouts are in the woods, not at a desk. The interface must be obvious at a glance.

2. **Respect the environment.** The app will be used outdoors, on phones with cracked screens, in low light, with cold hands. Large touch targets, high contrast, minimal precision required. The interface must work when conditions are bad.

3. **Earn trust through transparency.** Scouts should see what the system knows about them and their troop. No hidden states, no opaque processes. If a mission is pending approval, the scout sees that. If a vote is happening, the results are visible. Transparency is the default.

4. **Light weight, deep capability.** The app should feel fast and simple on the surface, but reward exploration. A new scout sees a clean dashboard. A Commander sees the full governance toolkit. The depth is there when you need it, invisible when you don't.

---

## 2. Foundation: Material Design 3

Adjutant builds on Material Design 3 (M3) as its base layer. M3 provides:

- **Adaptive layout** — works on phones, tablets, and desktop without separate codebases
- **Dynamic color** — the app's palette can adapt to the user's wallpaper or system theme
- **Comprehensive components** — navigation bars, cards, tables, forms, dialogs, badges, chips
- **Accessibility built in** — screen reader support, minimum touch targets, contrast ratios

We customize M3's tokens (colors, typography, spacing) to create Adjutant's unique identity while keeping the component behavior standard.

---

## 3. Color Palette

### Primary Colors

| Token | Hex | Usage |
|-------|-----|-------|
| Primary | #2E7D32 | Primary actions, active states, navigation highlights |
| On Primary | #FFFFFF | Text and icons on primary surfaces |
| Primary Container | #A5D6A7 | Subtle backgrounds, badges, status indicators |
| On Primary Container | #1B5E20 | Text on primary containers |

The primary green is the color of Vermont's forests — the environment where scouts spend their time. It signals "go," "active," "approved."

### Secondary Colors

| Token | Hex | Usage |
|-------|-----|-------|
| Secondary | #5D4037 | Supporting actions, secondary navigation |
| On Secondary | #FFFFFF | Text on secondary surfaces |
| Secondary Container | #D7CCC8 | Backgrounds for secondary content |
| On Secondary Container | #3E2723 | Text on secondary containers |

Warm brown — earth, wood, the trail. Used for supporting elements that don't need primary emphasis.

### Tertiary Colors

| Token | Hex | Usage |
|-------|-----|-------|
| Tertiary | #1565C0 | Information, links, external actions |
| On Tertiary | #FFFFFF | Text on tertiary surfaces |
| Tertiary Container | #BBDEFB | Informational backgrounds |
| On Tertiary Container | #0D47A1 | Text on tertiary containers |

Blue — the sky, water, information. Used for data display, links, and neutral information.

### Semantic Colors

| Token | Hex | Usage |
|-------|-----|-------|
| Success | #2E7D32 | Mission completed, vote passed, approval granted |
| Warning | #F57F17 | Pending action required, approaching deadline |
| Error | #C62828 | Rejected, failed, error state |
| Info | #1565C0 | Informational message, neutral notification |

### Surface Colors

| Token | Hex | Usage |
|-------|-----|-------|
| Surface | #FAFAFA | Main background (light mode) |
| Surface Dim | #F5F5F5 | Secondary backgrounds, cards |
| Surface Container | #EEEEEE | Input fields, dividers |
| Outline | #BDBDBD | Borders, dividers, subtle structure |
| Outline Variant | #E0E0E0 | Lighter borders, decorative structure |

### Dark Mode

| Token | Hex | Usage |
|-------|-----|-------|
| Surface | #121212 | Main background |
| Surface Dim | #1E1E12 | Secondary backgrounds |
| Surface Container | #2C2C2C | Input fields, cards |
| On Surface | #E0E0E0 | Primary text |
| Outline | #424242 | Borders |

Dark mode is essential for outdoor use — night hunts, evening meetings, low-light situations. The palette inverts cleanly because M3's token system handles it.

---

## 4. Typography

### Font Family

**Primary:** Inter (or system default)
- Clean, readable, excellent at small sizes
- Open source (SIL Open Font License)
- Designed for screens — good x-height, clear letterforms
- Variable font support for weight flexibility

**Monospace:** JetBrains Mono (for code, IDs, technical data)

### Type Scale

| Token | Size | Weight | Usage |
|-------|------|--------|-------|
| Display Large | 32sp | Bold | Screen titles (rare, hero moments) |
| Display Medium | 28sp | Bold | Section headers |
| Headline Large | 24sp | SemiBold | Page titles |
| Headline Medium | 20sp | SemiBold | Card titles, dialog titles |
| Title Large | 18sp | Medium | List item titles |
| Title Medium | 16sp | Medium | Subsection headers |
| Body Large | 16sp | Regular | Primary body text |
| Body Medium | 14sp | Regular | Secondary body text, descriptions |
| Body Small | 12sp | Regular | Captions, timestamps, metadata |
| Label Large | 14sp | Medium | Button text |
| Label Medium | 12sp | Medium | Chip text, tab labels |
| Label Small | 10sp | Medium | Badges, fine print |

### Rules

- Minimum body text size: 14sp (outdoor readability)
- Line height: 1.5× for body text, 1.2× for headings
- Maximum line length: 60 characters for body text
- No ALL CAPS except for labels and badges (and even then, prefer small caps)
- Dates and times use 24-hour format (scouts operate on mission time, not AM/PM)

---

## 5. Spacing and Layout

### Spacing Scale

| Token | Value | Usage |
|-------|-------|-------|
| xs | 4px | Tight spacing between related elements |
| sm | 8px | Default spacing between elements |
| md | 16px | Standard padding, section gaps |
| lg | 24px | Between major sections |
| xl | 32px | Page-level padding |
| 2xl | 48px | Hero spacing (sparingly) |

### Touch Targets

**Minimum touch target: 48×48dp.** This is non-negotiable. Scouts use the app outdoors, with gloves, with dirty hands, on bumpy terrain. Every interactive element must be large enough to tap reliably.

**Recommended touch target: 56×56dp** for primary actions.

### Layout Grid

- **Phone:** Single column, full width, 16dp horizontal padding
- **Tablet:** Two-column layout, 24dp horizontal padding, max content width 720dp
- **Desktop:** Sidebar navigation + content area, max content width 960dp

### Breakpoints

| Breakpoint | Width | Layout |
|-----------|-------|--------|
| Compact | 0-599dp | Phone — single column, bottom nav |
| Medium | 600-839dp | Tablet — two columns, navigation rail |
| Expanded | 840dp+ | Desktop — sidebar + content |

---

## 6. Components

### Navigation

**Phone:** Bottom navigation bar (3-5 items). Fixed, always visible. Icons with labels.
**Tablet:** Navigation rail (icons only, expandable). Left side.
**Desktop:** Sidebar navigation (icons + labels). Collapsible.

Navigation items are plugin-defined. The core provides the shell; plugins declare their nav entries via metadata.

**Items always present:**
- Home (dashboard)
- Missions
- Calendar
- Members
- More (overflow for less-used plugins)

### Cards

Cards are the primary container for content. They group related information and actions.

**Card anatomy:**
- Title (Title Large, bold)
- Subtitle (Body Small, muted color)
- Content area (Body Medium)
- Actions row (buttons, chips, badges)
- Status indicator (colored edge or badge)

**Card rules:**
- Rounded corners: 12dp
- Elevation: 0 (flat) by default, 2dp on press
- Padding: 16dp internal
- Maximum width: 400dp on phone, 320dp in grid

### Data Tables

Tables display structured data (member lists, mission rosters, financial records).

**Table rules:**
- Alternating row backgrounds (subtle: surface vs surface-dim)
- Sticky headers on scroll
- Sortable columns (tap header to sort)
- Row tap opens detail view
- Row height: 56dp minimum
- Checkbox column (if bulk actions available)
- No horizontal scroll on phone — use card view instead for narrow screens

### Forms

Forms follow a consistent pattern:

**Form anatomy:**
- Section header (Title Medium, bold)
- Field groups (label above input, 8dp gap)
- Input fields: full width, 48dp height, 16dp internal padding
- Helper text below field (Body Small, muted)
- Error text below field (Body Small, error color)
- Actions: primary button (filled) on right, secondary button (outlined) on left

**Field types (matching plugin metadata vocabulary):**
- Text: standard text input
- Long text: multi-line, expandable
- Number: numeric keyboard, validation
- Boolean: switch or checkbox
- Date: date picker
- DateTime: date + time picker
- Select: dropdown or bottom sheet (3+ options)
- Reference: lookup from another screen
- Badge: read-only status indicator
- Money: currency-formatted input
- File/Image: camera or file picker

### Badges and Status

Status indicators use color and icon:

| Status | Color | Icon | Meaning |
|--------|-------|------|---------|
| Active/Approved | Success green | ✓ | Mission approved, motion passed |
| Pending | Warning amber | Clock | Awaiting action |
| Inactive/Draft | Surface dim | — | Not yet submitted |
| Rejected/Failed | Error red | ✗ | Mission rejected, motion failed |
| In Progress | Tertiary blue | → | Active mission, ongoing work |

### Dialogs

**Alert dialog:** Title, body text, 1-2 action buttons. Used for confirmations, warnings.
**Bottom sheet:** Used for forms, selections, and detail views on phone. Drag to dismiss.
**Full-screen dialog:** Used for complex forms (mission proposal, motion creation).

**Dialog rules:**
- Maximum 2 actions (primary + secondary)
- Clear, specific action labels ("Approve Mission" not "OK")
- Destructive actions use error color and require confirmation

### Empty States

Every list and screen has an empty state — what the user sees when there's no data yet.

**Empty state anatomy:**
- Illustration or icon (simple, not decorative)
- Title (Headline Medium): "No missions yet"
- Description (Body Medium): "Create your first mission to get started."
- Action button (optional): "Create Mission"

Empty states are opportunities to guide new users. Never show a blank screen.

---

## 7. Icons

**Icon set:** Material Symbols (M3 default)
- Rounded variant (softer, friendlier than sharp)
- Consistent stroke width: 1.5dp
- Minimum size: 24dp, touch target: 48dp

**Plugin-specific icons:** Plugins may define custom icons, but they must follow the same size and style guidelines. Custom icons are registered via plugin metadata.

**Status icons (semantic, not decorative):**
- ✓ Checkmark (success/approved)
- → Arrow (in progress/forward)
- ! Exclamation (warning/pending)
- ✗ Cross (error/rejected)
- + Plus (create/add)
- ⋮ Overflow (more options)

---

## 8. Motion and Animation

**Principle:** Motion communicates state changes and guides attention. It should feel natural, not decorative.

**Transitions:**
- Page transitions: fade + slide (300ms, ease-out)
- Element appearance: fade in (200ms)
- Element removal: fade out (150ms)
- List item insert/remove: slide + fade (250ms)

**Micro-interactions:**
- Button press: scale down to 0.95 (100ms)
- Toggle switch: spring animation (200ms)
- Pull to refresh: standard Material refresh indicator
- Swipe to dismiss: standard Material dismissible

**Rules:**
- No animation longer than 400ms (feels sluggish)
- No animation shorter than 100ms (feels jarring)
- Respect system "reduce motion" setting — disable non-essential animations
- Loading states use skeleton screens, not spinners (skeletons communicate structure)

---

## 9. Offline-First

Adjutant works in the woods. Connectivity is not guaranteed.

**Visual language for offline state:**
- Subtle banner at top: "Offline — changes will sync when connected"
- Pending actions shown with a clock icon and "Pending sync" label
- Sync indicator: checkmark when synced, spinning arrow when syncing
- Conflict resolution: if offline changes conflict with server, show a clear comparison dialog

**Data architecture:**
- Client caches the last-known state locally (SQLite or Hive)
- Writes queue locally and sync when online
- Read operations always work (from cache)
- Write operations work offline and queue for sync

---

## 10. Accessibility

**Non-negotiable:**
- Minimum contrast ratio: 4.5:1 for body text, 3:1 for large text
- Touch targets: 48dp minimum
- Screen reader labels for all interactive elements
- Semantic HTML for web (PWA mode)
- Keyboard navigation for desktop
- Color is never the only indicator of status (always paired with icon or text)

**Age range:** Scouts range from 14+ (Pathfinders) to adults of all ages. The interface must work for a teenager on a small phone and an adult on a tablet.

---

## 11. Theming Architecture

### Design Tokens

All visual values are defined as design tokens — named constants that plugins and the core reference. No hardcoded colors, sizes, or text styles in widget code.

```dart
// Example token usage
Container(
  color: AppColors.primaryContainer,
  padding: EdgeInsets.all(Spacing.md),
  child: Text(
    'Mission Approved',
    style: AppTextStyles.titleMedium,
  ),
)
```

### Theme Extension

Adjutant uses Flutter's `ThemeExtension` to add custom tokens to Material's theme:

- `AppColors` — semantic color tokens (success, warning, error, info)
- `AppSpacing` — spacing scale tokens
- `AppRadius` — border radius tokens
- `AppShadows` — elevation tokens

### Plugin Theming

Plugins declare their UI metadata, but the client controls the visual rendering. A plugin cannot change the color palette, typography, or spacing. This ensures visual consistency across all plugins.

---

## 12. Component Catalog

The design system is documented as a Widgetbook component catalog. Each component has:

- **Name:** What it's called
- **Purpose:** When to use it
- **Anatomy:** Visual breakdown of parts
- **States:** Default, hover, pressed, disabled, loading, error
- **Do / Don't:** Usage guidelines
- **Code:** Flutter widget with token-based theming

**Catalog index:**

1. Navigation (BottomNav, NavRail, Sidebar)
2. Cards (Basic, Interactive, Status)
3. Data Tables (Sortable, Selectable, Responsive)
4. Forms (Text, Select, Date, Boolean, Composite)
5. Badges and Status Indicators
6. Dialogs (Alert, Bottom Sheet, Full-Screen)
7. Buttons (Filled, Outlined, Text, Icon, FAB)
8. Empty States
9. Loading States (Skeleton, Progress, Refresh)
10. Offline Indicators
11. Header and Toolbar
12. List Items (Basic, Expandable, Swipeable)
13. Chips and Tags
14. Search and Filter

---

## 13. Inspiration and References

| Source | What to borrow | Why |
|--------|---------------|-----|
| Material Design 3 | Component library, adaptive layout, token system | Foundation — Flutter-native, comprehensive, accessible |
| Widgetbook | Component catalog and documentation pattern | Maintains the design system as code |
| FlareLine (Flutter admin) | Sidebar navigation, data table patterns, dashboard layout | Proven patterns for data-heavy admin interfaces |
| Nextcloud Deck | Task cards, kanban-style workflow, group collaboration | Closest analog to mission/task management |
| Loomio | Consensus decision-making UI, proposal/vote flows | Direct analog to governance plugin |
| HelpButtons | Community-facing, cooperative network design | Same ethos as Adjutant — community-owned |
| Obsidian (mobile) | Clean information density, dark mode, offline-first | Information-heavy, works in any environment |
| Signal | Minimal, trustworthy, security-first design | Earns trust through simplicity and transparency |

---

## 14. Open Questions

1. **Branding:** Should Adjutant have its own logo and visual identity, or remain unbranded (generic enough for any troop to adopt)?
2. **Customization:** Should troops be able to customize the color palette (their troop colors) or is a fixed palette better for consistency?
3. **Illustrations:** Should the empty states and onboarding screens use custom illustrations (outdoor/scouting themed) or stick to Material icons?
4. **Onboarding:** What's the first-run experience? How does a new scout learn the app without a tutorial?
5. **Notifications:** Push notification design — how do they look, what information do they carry, how do scouts control them?

---

*This design language is a living document. It evolves as the Flutter client is built and tested with real scouts.*
