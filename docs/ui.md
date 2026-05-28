# UI design

3drip is a libadwaita-first GTK4 application. It targets GNOME 46 / 47
HIG: adaptive layouts, `AdwApplicationWindow`, `AdwNavigationView`,
`AdwToolbarView`, `AdwPreferencesWindow`, `AdwStatusPage`, and
`AdwBanner`.

The window adapts to phone widths (one-pane navigation, collapsing
sidebars) and desktop widths (overview-first layout).

## Top-level flow

```
   ┌────────────────────────────┐
   │      WelcomeOverview       │   AdwOverviewSplitView
   │  ├─ no disc inserted        │
   │  ├─ disc inserted →         │
   │  │   click "Identify"       │
   │  └─ recent jobs list        │
   └────────────┬───────────────┘
                ▼ identify
   ┌────────────────────────────┐
   │   IdentificationPage       │
   │  · spinner while hashing    │
   │  · TheDiscDB result card    │
   │  · "Wrong release?" chooser │
   │  · or: Unidentified flow ──→│  ⟶  ManualEntryPage / SubmitPage
   └────────────┬───────────────┘
                ▼ confirm
   ┌────────────────────────────┐
   │   TitleSelectionPage       │
   │  · table of titles          │
   │     index · duration · size │
   │     role · selected         │
   │  · per-title role override  │
   └────────────┬───────────────┘
                ▼
   ┌────────────────────────────┐
   │   OutputPlanPage           │
   │  · naming scheme picker     │
   │  · library root             │
   │  · transcode preset (opt-in)│
   │  · 3D layout (if MVC)       │
   └────────────┬───────────────┘
                ▼
   ┌────────────────────────────┐
   │   JobRunPage               │
   │  · per-stage progress bars  │
   │  · log expander             │
   │  · cancel / pause           │
   └────────────────────────────┘
```

## Pages

### WelcomeOverview

`AdwOverviewSplitView` with the disc drive list on the left and a
content pane on the right. When a disc is inserted, the content pane
shows an `AdwStatusPage` with the disc label, type (DVD/BD/UHD), and a
big **Identify disc** button.

A "Recent jobs" `AdwPreferencesGroup` at the bottom shows the last 5
jobs with their state and a "view" action that reopens the job page.

### IdentificationPage

`AdwNavigationPage` pushed from Welcome. During the lookup it shows a
spinner with `AdwStatusPage` ("Hashing disc…", "Querying TheDiscDB…",
"Looking up metadata…"). On success, an `AdwClamp` with:

- cover art (`Picture` from TheDiscDB's `imageUrl`)
- title, year, type
- release region / locale / UPC
- "Continue" button → TitleSelectionPage
- "Wrong release?" link → release chooser modal

On zero matches, an `AdwStatusPage` with `dialog-question-symbolic`,
text "We couldn't identify this disc", and a primary
"Enter details manually" button leading to ManualEntryPage.

### ManualEntryPage / SubmitPage

Single-pane form (AdwPreferencesGroup):

- Title (entry)
- Year (entry, numeric)
- Type (`AdwComboRow`: Movie / TV Series / Documentary / Concert / Other)
- Region (`AdwComboRow`: A / B / C / 1–8 for DVD)
- Locale (combo)
- UPC (optional entry)
- TMDB lookup button → modal search → fills IDs

Below: the per-title role table (re-used from TitleSelectionPage).

Submit button is dual-action:
- **Save locally and continue** → uses the entered data for this rip
  only.
- **Submit to TheDiscDB** → opens a pre-filled GitHub PR in the
  default browser (`xdg-open`), or hits the contribution endpoint when
  available. A toast confirms.

### TitleSelectionPage

A `Gtk.ColumnView` with rows per title:

| ✓ | # | Duration | Size | Role | Source file | Languages |
|---|---|----------|------|------|-------------|-----------|

The role column is an `AdwComboRow`-style cell with the
main/trailer/featurette/… options pre-populated from TheDiscDB and
editable.

"Select main feature only" / "Select all main + extras" / "Select all"
quick actions at the top.

### OutputPlanPage

`AdwPreferencesPage` with groups:

- **Library**
  - Naming scheme (`AdwComboRow`: Jellyfin / Plex / Kodi / Emby)
  - Movies root (`Gtk.FileChooserButton` via FileDialog portal)
  - Shows root (same)
- **Encoding**
  - "Rip as-is" / "Transcode with preset" toggle
  - Preset picker (only if transcode enabled)
  - "Edit presets…" link → external editor of TOML in user data dir
- **3D (only shown if MVC track present)**
  - Layout (`AdwComboRow`: Full-SBS / Half-SBS / Full-TAB / Half-TAB /
    Frame-Sequential / Interleaved)
  - Decoder (`AdwComboRow`: Britz FFmpeg / JM reference / Wine+FRIM —
    populated by runtime detection)
  - Subtitle depth (`AdwComboRow`: passthrough / hardcoded with depth /
    skip subtitles)

A preview pane at the right shows the planned target paths for the
selected titles, updating live as the user changes scheme/root.

### JobRunPage

Per-stage `AdwExpanderRow`s:

1. Ripping (with AdwProgressBar, current title indicator)
2. Transcoding (only if enabled)
3. 3D composition (only if MVC)
4. Naming and moving files

A log expander at the bottom shows `tracing` output for the current
job; the "Save log…" action exports the full log to disk.

`AdwBanner` at the top shows transient errors (drive busy, network
gone) without blocking.

## Application-level UI

- **Application menu**: Preferences, Keyboard Shortcuts, About 3drip
- **Preferences**:
  - Default naming scheme
  - Default library roots
  - Default transcode preset
  - TMDB API key (stored via libsecret)
  - MakeMKV beta key shortcut (deep-link to MakeMKV settings file)
- **About**: AdwAboutDialog with credits, GPL-3 notice, links to docs

## HIG adherence

- All padding follows the 12/24 spec.
- No custom widgets where libadwaita has one (no hand-rolled toggle
  rows, no Gtk.Dialog where AdwDialog applies).
- Symbolic icons throughout; no colour-laden custom icons in the
  chrome.
- Keyboard: every primary action has an accelerator; the column views
  support full keyboard nav.
- Accessibility: every interactive widget has an a11y label, every
  status page has descriptive text, progress bars announce changes.
- High contrast tested via `gsettings set org.gnome.desktop.a11y.interface
  high-contrast true`.
- Cursor focus tracking, no focus traps.

## Blueprint vs .ui

We use [Blueprint](https://gnome.pages.gitlab.gnome.org/blueprint-compiler/)
(`.blp`) for all UI definitions. Meson compiles `.blp` → `.ui` and bundles
both into the `.gresource`.
