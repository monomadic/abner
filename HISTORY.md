# abner — history

Completed work, newest first. Task numbers refer to [TASKS.md](TASKS.md) where a task
existed there before it landed; earlier entries predate the task list.

## 2026-10-14 — positioned copies and separate restore selection

Option-drag now inserts the copy at the drop boundary: before/after the card
under the pointer, or at the end when dropped in blank space below the cards.
Clicking a removed timeline region now selects it for restoration independently
of explicit in/out marks. Previously that click silently assigned both marks,
which made the colored handles appear when clicking dashed blank areas. The
selected removed region is outlined and X restores it without changing user marks.

## 2026-10-14 — selection deletion and Option-drag copies

Backspace in Sources now closes all checked clips, falling back to the focused
clip when none are checked. Survivors retain their playback time and focus when
possible. Option-dragging a source within the pane inserts an independent linked
copy immediately after it on release. A click without movement or a cancelled
drag creates no copy; unrelated checked sources are not duplicated.

## 2026-10-14 — Backspace closes a source

In Sources, Backspace closes the focused clip through the existing close path,
keeping survivors synced and returning to the launch window after the last clip.
Backspace on the empty window does nothing. Timeline deletion and Mask/Crop speed
reset retain their existing behavior.

## 2026-10-14 — clip preview toggle

The movie icon beside the video lane now toggles the leading clip previews.
Previews start visible; hiding them also stops their hover playback. The icon
dims when hidden and its tooltip names the next action. The setting lasts for
the current app session and leaves frame-detail cards and chapter thumbnails alone.

## 2026-10-14 — re-encode tint follows the clip

Purple re-encode regions now tint the full-height clip background, with square
internal GOP boundaries and rounded corners only at the clip's own ends. The
stripe pattern follows the original clip, and frame markers stay above the tint.
Encode buttons retain their shared drawing/hit geometry; the close-zoom encoding
rail is unchanged.

## 2026-10-14 — split-guide video preview

While the split guide is visible, the canvas displays its exact source frame using
a separate native-resolution decoder. Moving the guide reuses that decoder and
replaces pending seeks; the master clock and playback state remain untouched.
Leaving the guide or disabling the tool restores the normal playhead frame.

## 2026-10-14 — hover playhead preview

Hovering a scrubbable part of the timeline shows a translucent playhead pin and
line at the click target. Preview and scrub share the same time calculation,
including frame-card snapping. It hides everywhere while the split tool is on,
and during drags and over trim grips, removed clips and controls.

## 2026-10-14 — frame-marker height hierarchy

Zoomed timeline ticks now share an inset bottom baseline: B frames are short,
P and I frames progressively taller, and keyframes tallest, following the original
design's height rhythm. Existing frame colors and closer-zoom cards are preserved.

## 2026-10-12 — native View menu

The macOS menu bar now lists Sources, Timeline, Mask and Crop under View.
Selections use the header's existing mode transitions; a checkmark follows the
actual workspace, including toolbar and keyboard changes. Mode commands are
disabled on the empty launch screen. AppKit queues actions into the event loop
instead of mutating the player from a native callback.

## 2026-10-12 — clean background beneath frame markers

The video poster and diagonal clip fill disappear at the same visible-frame
threshold that reveals individual frame markers, rather than waiting for frame
cards. Zooming back out restores the clip treatment. Audio is unchanged.

## 2026-10-12 — Figma SVG frame icons and card fidelity

Replaced the hand-drawn frame symbols with the original Figma SVG card variants,
bundled and rendered locally. Frame labels and numbers now use JetBrains Mono
Medium with the design's tracking and icon spacing. Cards fit their image area
to the source aspect ratio, removing extra internal padding without stretching
or cropping the picture. Narrow cards retain readable type and omit the number.

## 2026-10-12 — GOP thumbnail inspection

Implemented amended Figma board 258:496: bright frame cards with the new diamond
icon language, actual per-frame previews, and Decoded / Changes controls. Changes
whitens unchanged pixels relative to the previous decoded frame; keys stay whole.
Preview jobs and atlas storage are bounded, and thumbnails retain source aspect.

Zoom stops at 160 pixels per average frame. At thumbnail detail, selecting video
selects its GOP, shows frame counts, and makes Backspace remove only that GOP.
Wider views keep clip selection. Regression coverage checks real decoded pixels,
change masking, zoom limits, GOP deletion/undo and compact layout.

## 2026-10-12 — remove the re-encoding visibility switch

Removed the toolbar switch and R binding after design review. Purple pending
regions and Encode controls remain available, including the separate strip at
close zoom. Frame cells, colors and the green keyframe-snap state are retained.
Lossless/lossy mode controls are deferred to a future design.

## 2026-10-12 — outlined frame cells and toolbar toggle states

Matched the revised Figma palette and frame-cell treatment: green keys, yellow I,
magenta P and cyan B; close zoom reveals outlined cells and type labels, then
source frame numbers. Actual timestamp intervals set cell widths, including VFR,
and partial cells at the viewport edge stay visible. The clip poster gives way
to frame cells at close zoom.

Snap now shows a green selected state. The RE-ENCODING OFF/ON switch and R key
show/hide pending regions and Encode controls without changing edits or jobs.
At close zoom, the purple encoding strip sits below the video cells. Narrow
windows move Encode all to the header so the switch and clock remain usable.

## 2026-10-12 — frame-aware split encoding and natural pinch zoom

Fully analysed I/P-only GOPs now copy the prefix before the first split and encode
the suffix through the next keyframe, adding keys at every split in that suffix.
GOPs containing B-frames, unknown types or incomplete analysis still encode whole.
Purple regions and Encode buttons use the same planned suffix. The worker verifies
the source frame types again before copying a partial GOP and retains frame timing,
audio and full-decode checks before publishing the working source.

Regression coverage includes multiple splits, incomplete metadata, B-frame rejection,
constant/variable frame rates, identical copied picture slices and unchanged audio.
Timeline pinch now matches the video canvas: spread fingers to zoom in, close them
to zoom out, preserving the source time under the pointer.

## 2026-10-12 — colored frame detail at close zoom

Timeline video clips now show tall amber keyframe markers, gray non-key I frames,
blue P frames and shorter mint B frames, matching Figma Frame marker 233:411.
Ticks appear at 10px per frame, I/P/B labels at 32px, and zero-based source frame
numbers at 80px. Zoom reaches 112px per frame. The compact track height and single
clip poster remain; this does not introduce a per-frame filmstrip.

A separate cancellable ffprobe worker streams decoded picture types and key flags
without delaying the packet index or editing. Unknown types remain neutral until
analysis arrives. Working-source replacement cancels the old reader and clears its
metadata. Markers use source presentation timestamps, including variable rates;
purple encoding overlays remain above the markers.

## 2026-10-12 — working split-GOP encoding and frame markers

The renderer conservatively processes the whole affected GOP, including the leading
piece; minimizing that span requires frame-dependency analysis.
Purple hatched regions identify GOPs crossed by kept clip boundaries, independently
of the snap toggle. Each visible region has Encode; the transport has Encode all.
Jobs run off the UI thread, deduplicate boundaries in the same GOP, and reject
concurrent submissions. A cancelled or failed job leaves the source unchanged.
Completed jobs publish a uniquely named `.encoded-*.mp4` beside the original and
switch the active source/player to it without moving the timeline. Originals stay
intact. Subsequent edits, undo and export use the working file's rescanned keys.

The initial smart-render backend supports zero-based 8-bit H.264 4:2:0 sources,
including B-frames and variable frame rates. It copies untouched GOPs, encodes
requested GOPs with closed IDRs at edit boundaries, and copies audio/subtitles
separately. Other codecs report an error; regular frame export remains available.
Before publishing, it verifies frame count, every presentation timestamp, dimensions,
keyframe placement and decoding. Intermediate files are removed on failure/cancel.
The working file is persistent; this is not a destructive overwrite of the original.

Actual frame timestamps drive frame snapping and video-lane markers. Markers appear
only when neighbours are at least 10 logical pixels apart; lookup is limited to the
visible range. Pending-GOP lookup follows edit boundaries instead of scanning the
entire source on every redraw. The export readiness label now follows actual edits.

Validated selective frame changes, identical untouched frames/audio, B-frame and
VFR timing, cancellation cleanup, pending-region undo and the real button/worker/
player replacement path. Targeted native capture verifies purple regions, inline
Encode buttons, toolbar spacing and frame markers. The UI inspection connector
stalled; the repository's targeted window-capture path completed the visual check.

## 2026-10-12 — toggleable split tool and re-encode design proposal

The Split toolbar button now toggles an armed tool. Its circular split marker
and dashed line preview the exact click target: nearest keyframe with snapping,
or the next nominal frame boundary without it. Clicking splits without moving
the playhead; the tool stays armed, and Escape or the button turns it off.
The existing S shortcut still splits directly. Added preview/click, snapping,
repeat-use and Escape regression coverage; all 72 tests pass.

Figma proposal 223:382 in the Timeline Edit file shows purple hatched re-encode
regions with per-region Encode buttons. This is a design proposal for in-place
GOP processing, not an implemented smart-render pipeline; current frame-mode
export still re-encodes the whole output.

## 2026-10-11 — stable clip presses and pointer seeks

Timeline mouse-down keeps the clip hover lift and thumbnail decoder active
through scrubbing instead of fading/restoring and restarting them on release.
Pointer seeks keep their requested clock position; only discrete seeks and
frame steps adopt the first decoded frame timestamp, once. Repeated pointer
positions no longer reissue timeline seeks. Regression coverage checks held
hover, thumbnail continuity, and a stable playhead across real decoder delivery.

## 2026-10-11 — inspector typography, Info tab, and GOP details

Matched the right inspector to Figma's SF Pro labels and section headings,
JetBrains Mono values, 22.3px rows and 16px section gaps. Tabs now read
Chapters, Subtitles, Segments, Info, with a circled info icon and hollow
keyframe diamonds. Info scrolls independently inside its fixed header/footer.
The old Count was the file's total keyframes; it is now labeled Keyframes.
The current GOP reports its index, encoded packet/frame count, duration and
video payload size in B/KiB/MiB, alongside median and longest GOP durations.
Packet counts are explicitly qualified, and sizes exclude audio/container overhead.

## 2026-10-11 — reference-video summary typography and spacing

Matched Figma 109:379 with bundled JetBrains Mono Regular at 12px/10px,
a 3px white thumbnail border, and 14px below the preview before the divider.
Short windows keep the same summary spacing rather than collapsing the gap.

## 2026-10-11 — inset selection outlines and hollow keyframe icons

Selected video and audio clips now draw their 2px white border inside the
original rounded shape, over clip contents, so the gutter cannot cut off an
outward stroke. Previous/next keyframe buttons use hollow diamonds and chevrons.

## 2026-10-11 — thumbnail joins and right-aligned transport clock

Timeline posters round only their left corners, with a square join to the clip
and a faint permanent white border instead of a hover outline. Other thumbnail
surfaces retain their rounded cards. Delete now uses an X, and the timer plus
its controls and divider move together beside the right-hand zoom controls.

## 2026-10-11 — rounded clip trim highlights

Trim highlights now use clipped slices of the clip's rounded silhouette instead
of square strips that overpainted its corners. Both video and audio ends keep
the Figma 7px radius and use the design's inset, slimmer paired grip lines.

## 2026-10-11 — text-only mode-selector hover

Hover now turns the mode label white without adding a background fill.
Only the selected mode retains the gray button fill, across all workspaces.

## 2026-10-11 — latest filled mode selector

Synced Figma 122:441 again: 385×26 dark filled housing, 2px padding, 4px gaps,
equal-width buttons, borderless gray selection and uppercase 11px JetBrains
Mono Medium. Bundled the official font and OFL license so all installations
use the same face. The shared component applies the update to every mode.

## 2026-10-11 — one shared Figma mode-selector component

Applied the updated Figma container 122:441: transparent housing with a 2px
neutral outline, gray selected fill, and SF Pro Display Medium 12px labels.
ModeSelector owns drawing and control geometry in every workspace; Timeline
no longer relocates the selector. The installed Display Medium face is preferred,
with system SF Pro at medium/display optical size as the fallback. Other interface
labels retain SF Compact Rounded. Mode-switch tests guard geometry consistency.

## 2026-10-11 — interface typography and crisp glyph placement

Verified the newer Figma label (122:444) uses SF Compact Rounded Regular at
12px. Interface labels now use that macOS font at weight 400 with em-based
sizing; the main timecode and explicitly authored mono runs retain their font.
Glyph quads align to physical pixels, avoiding a second fractional filtering
pass over rasterized coverage. Atlas keys include the face and exact raster
size, so adjacent fractional sizes cannot accidentally reuse one bitmap.

## 2026-10-10 — neutral title-bar mode selector

All four workspace modes now share the reference's dark rounded housing,
equal-width buttons, gray selected fill and border, and neutral title-case labels.
Drawing and pointer hit testing use the same inset button geometry.

## 2026-10-10 — subtitle and segment inspector tabs

Expanded the inspector selector to Chapters, Streams, Subtitles and Segments.
The new tabs show timed text cues and kept/cut ranges, with active-row highlighting
and click-to-seek. Long lists follow the playhead, like the chapter list.

## 2026-10-10 — inspector outline icons

Updated the inspector to the newer notched chapter flag, divided filmstrip,
slim list and rounded grid icons. Selected tabs and toggles have a subtle blue
outline, and the add button precedes the view toggle as in the reference.

## 2026-10-10 — full-height timeline posters

The leading video-clip area now displays its thumbnail at full track height,
under the trim grips, instead of an inset card. Short clips keep a cropped
poster rather than losing it; existing hover playback remains available.

## 2026-10-10 — timeline track-control gutter

Matched the reference's darker 49px gutter, subtle divider and muted outline
icons: bookmark, filmstrip, speaker with sound waves, and boxed A. Removed the
extra keyframe-row icon. Media clipping and track interactions are unchanged.

## 2026-10-10 — compact timeline track spacing

Applied the compact Figma track offsets: ruler at 2px, chapters at 20px, video
at 48px, audio at 108px, subtitles at 156px and keyframes at 180px. The timeline
is now 198px high (previously 222px), reclaiming space for the preview. Short
windows use the same tighter ruler gap in a 126px timeline. Drawing and hit
testing continue to share the lane geometry.

## 2026-10-10 — one split button, replacing scissors

Removed the duplicate scissors button. The outward-arrow split icon is now the
only split control and also marks the timeline razor. S keeps its existing
pointer-aware split behavior; clicking the button splits at the playhead.
Closed the toolbar gap and moved the time display left with the five-tool group.

## 2026-10-10 — keep selection outlines behind the track gutter

Kept the selected clip outline inside the timeline's content clipping region.
Panning a selected video or audio piece past the left edge now leaves the track
labels unobscured while retaining the outline above and below the clip.

## 2026-10-10 — split icon reference

Matched the split-at-playhead icon to the supplied reference: sharp outward
triangles and three separated center dashes. Its split behavior is unchanged.

## 2026-10-10 — delete picked timeline clips and refine toolbar icons

Backspace deletes the picked video/audio timeline piece through the existing cut
model, with undo; the trash button also acts on the picked piece. Updated in/out
to open arrowheads, keyframe navigation to filled diamonds with chevrons, and the
trash icon to a tapered outline, matching the supplied references.

## 2026-10-10 — current Figma transport layout

Matched Transport Toolbar 73:111: playback, six unboxed edit tools, then the
larger TIME/FRAMES display, with the design's spacing and dividers. Removed the
extra snap lamp and matched the outlined snap diamond. Zoom stays right-aligned
and hides at compact widths. Existing actions, shortcuts and hover labels remain.

## 2026-10-09 — consistent unselected chapter flags

Timeline chapters before and after the playhead now share the solid grey flag
and a 3px stem reaching the video track, matching the supplied reference.
The current chapter retains its amber treatment.

## 2026-10-09 — simpler keyframe snap icon

Replaced the magnet with a keyframe diamond centered on a vertical snap line in
both the Rust toolbar and Figma. The existing K shortcut and snap behavior remain.

## 2026-10-09 — split-at-playhead toolbar button

Added the supplied outward-arrow/dashed-playhead icon beside the scissors in the
Timeline toolbar. It splits at the current playhead using the existing snap and
undo behavior. The scissors and S shortcut retain pointer-position splitting.

## 2026-10-09 — transport matched to the supplied reference crop

The reference shows five playback controls (keyframe, frame, play, frame,
keyframe), a separator, TIME/FRAMES, another separator, and five tight edit
buttons. The transport now uses that same order and spacing at every window
width. Chapter navigation stays in the inspector; the toolbar no longer shows
chapter buttons. Play/pause uses black marks on blue; split and snap now draw
scissors and an outlined magnet to match the reference. Drawing and hit testing
share the control rectangles.

## 2026-10-09 — Timeline transport styling

Matched the selected Figma `transport/toolbar` frame at 1440px: black 50px ground,
more widely spaced playback controls, labeled time and frame readouts, and edit
tools shifted right. The shared control rectangles still drive both drawing and
hit testing. Narrow windows keep the compact arrangement, including chapter
navigation. The existing geometric icons and snap/zoom controls remain live.

Follow-up: the first pass only displayed the new playback treatment at 1400
logical pixels, so a normal half-screen window still looked unchanged. The
compact transport now uses the Figma's unboxed playback icons and labeled
timecode too. Below 900px it hides the zoom strip (wheel zoom still works),
leaving the edit tools room without overlap.

## 2026-10-08 — updated Timeline Edit page

Implemented the updated Figma Main 12:2 layout: compact centered Timeline header,
wide Export button, black five-button transport, TIME/FRAMES readout, outlined
editing controls, and the darker track ground. The preview uses the board's inset
proportions, with stacked keyframe/current-chapter tags. The inspector now begins
with a live source thumbnail, filename and metadata above Chapters/Streams; list,
grid and add controls share their drawing and input geometry. Chapter rows remain
paged clear of the footer rather than reproducing the mock's footer overlap.

Backtick switches time/frame display without moving the playhead; `{` / `}` keep
chapter navigation available after its toolbar buttons were removed in the design.
Undo remains Cmd-Z / U. `--cut --timeline-frames` exposes the frame readout for
captures. Sources/Crop/Mask keep their existing header style. The new Timeline
colors are mirrored as documented Figma variables. Build and all 68 tests pass,
including inspector navigation, linked selection behavior and the display toggle.
Final native screenshot verification is pending: macOS was locked during review.

## 2026-10-08 — Sources preview cards and header input routing

Matched Figma's Sources preview cards (81:95): full-width live previews, number
and ACTIVE badges, filenames and metadata underneath, and a source-count chip.
Cards are 184px tall with 12px gaps in a 280px rail; compact windows stack metadata.
Batch checkboxes appear on hover or when checked. Linked duplicates retain shared
file labels, and Duplicate/Add files move to the rail footer. The inspector returns
at 800px high. Existing duplication and multi-select behavior is preserved.

A native input overlay routes the exact painted header control regions to winit,
preventing AppKit's transparent titlebar from consuming button clicks as window
zoom/drag gestures. Empty header space and traffic lights retain native handling.
Build and all 68 tests pass. Live window checks confirmed mode-button single and
double clicks keep window size, while empty-header double clicks zoom and restore.
The Sources layout was visually checked at 1280×800 and in the zoomed window.

## 2026-10-08 — native titlebar double-click

The transparent full-size content view receives empty-header clicks as client
input. Runner now recognizes double-clicks using the macOS interval and calls
AppKit's zoom/restore action (or minimize/no action according to the system
preference). Tab housing, header actions, hidden controls and fullscreen are
excluded. Moving away, dragging or losing focus cancels the pending click.
Build and all 67 tests pass, including header hit regions and gesture timing.

## 2026-10-08 — Sources cards and linked duplicates

Implemented the Figma processing proposal's source rail: live rounded thumbnails,
readable metadata, item/file counts, linked-copy labels and separate viewed/checked
states. Cards keep their geometry for drawing, clicking and scrolling; resizing
keeps the viewed card visible. The 320px rail narrows at 900px; the lower inspector
now yields below 1000px high so normal windows fit three source cards.

Duplicate / Cmd-D duplicates checked items (or the viewed item) without copying
media on disk. Copies have independent decoder/mask state; paused and playing
insertion preserve master time and re-deliver shifted texture slots. X toggles the
viewed checkbox, Cmd-A selects/clears all and Clear resets batch scope. Add files /
Cmd-O uses a non-modal native multi-file picker through the Open With queue.
`--duplicate` exposes the state for visual review. Topaz and batch processing remain
Figma proposals. All 65 tests pass, including new selection, resize, paused/batch
and playing duplicate-sync coverage. Targeted window captures verified the Sources
rail at 1440×900 and 720×480; the compact view keeps the viewed copy visible.

## 2026-10-08 — updated shared header alignment

Refreshed Figma Header `12:5`: the mode selector is now left aligned at x=85
(x=14 in fullscreen), 356px wide, with Sources / Timeline / Crop / Mask labels.
All four modes share the 44px header, outlined housing and blue active button,
so switching modes keeps the selector in place. Timeline retains its existing
Undo/Export controls and status. Drawing and hit testing use the same controls.

## 2026-10-08 — Timeline Edit header

Implemented Figma Timeline Edit header `12:5`: a 44px #050506 bar,
31px outlined tab housing, blue TIMELINE selection, green stream-copy status, and
right-aligned Undo (with ⌘Z keycap) / Export controls at the board's sizes.
The clip filename leaves the header; it remains in the native window title.
The timeline canvas and inspector start below the new header. The status yields
below 1000px so the tabs and actions fit at 720px; native traffic lights retain
100px clearance (14px in fullscreen). T / Escape still return to Sources.
The header keeps Abner’s SOURCES, TIMELINE, CROP and MASK modes, wired to
their existing actions. The existing mono text renderer is retained.
Undo/export keep their existing availability and actions. All 62 tests pass;
window captures verified the header at 1440×800 and 720×480. The existing
scrub-toolbar overlap at 720px is outside this header change.

## 2026-10-07 — the scrub panel brought level with the Figma board

Implemented from the Figma file "Abner — Timeline Edit", node 25-24 (the toolbar and
media timeline), against the build of 2026-10-05. The geometry was already the board's;
this is the detail pass:

- **Grounds**: the toolbar is `CUT_BAR` (#100f0e) and the lanes `CUT_TL` (#1a1a18), the
  board's two surfaces. The tool buttons carry the board's 1px #232323 border at an 8px
  radius; the zoom −/+ are its magnifiers (`Icon::ZoomOut`/`ZoomIn`, the handle a stair
  of squares — no diagonal primitive). The snap lamp is #20c45f at 42% with a faint halo.
- **Chapters** have three states: passed (a #393a3a flag, 2px pole down to the clips),
  current (amber, a short pole), still to come (a faint flag over a 6px hairline). The
  pole no longer runs up past the flag.
- **Clips**: the sound row is 39px (was 52) and a sound clip is the board's vertical
  gradient (#112a42 → #0c1d2f, `fade_down`) under its blue border; waveform peaks scale
  to the row with a 6px floor. Each end of a clip has the 8px trim glow and the grips
  inside it (38px on picture, 27 on sound). An idle thumbnail keeps a 16% white border
  at a 5px radius. Cues are #c1a9ee.
- **Marks**: in is a 5px bar with a foot at the top and the bottom; out keeps its tab. The
  snapped keyframes stand tall at 1px. The razor's disc sits 4px higher. The playhead's
  pin is the board's 11×13 tab with three dark grooves, and its timecode is a WHITE chip
  with black type centred under the line on the keyframe row, not a dark one beside it
  on the ruler.
- **Dark type on light chips** (flag numbers, the timecode chip, the lit tool buttons)
  read brown/grey: glyph coverage blends in linear light, so a thin black stroke's
  edges over amber came back far too bright. Shader mode 6 now lifts a dark glyph's
  coverage by the same `1 − (1 − a)^2.2` the launch overlays use (faded in below ~0.3
  luminance, so light type on dark ground is untouched), and the flag numbers are
  drawn twice half a pixel apart to stand in for the board's Bold weight.
- Not taken from the board: it draws only the play disc before the clock, where the
  toolbar keeps the chapter / keyframe / frame skips added since (they get the same
  border); the gutter icons are white as drawn there, but built from rects as before.

## 2026-10-06 — a lone file opens on the timeline

Launch with one file, drop one on an empty window, or open one from Finder / recents, and
Abner opens in TIMELINE mode; two or more files stay in SOURCES (the compare view).
`App::set_cut_default(true)` (main.rs) turns it on and `settle_cut_default` runs at launch
and after every drop (replacing with one file counts); `--mask` and `--crop` still take
their own tool, and tests leave the flag off so Sources stays what they exercise. A file
with no duration stays in Sources (the timeline needs one).

## 2026-10-06 — toolbar tabs renamed and reordered

`INPUT` → `SOURCES`, `CUT` → `TIMELINE`, shown as SOURCES · TIMELINE · CROP · MASK (the
status line's mode word follows). Only the labels and their order moved: `Action::Tool`
indices are unchanged (0 sources, 1 mask, 2 crop, 3 timeline), so nothing keyed on them
did. The code still says "cut mode" and "input mode" internally.

## 2026-10-06 — Cut mode: `+` adds a chapter, and the export writes them

A `+` beside the list / thumbnails toggle starts a chapter at the playhead ("Chapter N",
refused within half a second of an existing one). The export now carries chapters:
`cut::output_chapters` moves each by what was cut before it and drops those whose start
is inside a cut, and an ffmetadata input (`-map_chapters 1`, `-map_metadata 0` so the
file's own tags survive) writes them for both the stream-copy and the re-encode paths.
Adding a chapter alone is enough to export. No rename or delete yet, and no undo for them.

## 2026-10-06 — Cut mode: U undoes, the wheel zooms the track, a backgrounded app idles

`U` is undo as well as ⌘Z. Over the timeline, scrolling up zooms in and down zooms out
about the pointer (exponential, `cut.zoom`), a sideways swipe pans, pinch still zooms.

Cmd-tabbing back to a PLAYING clip was slow while a paused one was instant. The
difference is the loop: playing runs the Poll cadence, presenting 4K frames as fast as the
display allows, and the OS's activation work waits behind it. A backgrounded window
(`Focused(false)`) now schedules like an occluded one — idle ticks, no Poll — and its
clock is held (`dt = 0`), so playback pauses where it was instead of running on unseen;
focusing again restarts the clock and redraws at once. Not measured: this was found by
reading the loop, not by profiling, because the capture tooling here sees a black screen.

## 2026-10-06 — cut mode: the chosen inspector, a thinner playhead

The right-hand panel is the "Cut inspector (chosen)" board: the Selection block is gone
(the context row and the timeline's in/out ghosts already say it), and Segments became
chapters. Two icon tabs on top — chapters and streams. **Chapters**: a chip strip of the
stream facts (`H.264 · 2160p · 23.976 · AAC 2ch · SRT`), a count, a list / thumbnails
toggle, and one row (or card) per chapter, the one under the playhead in the highlight
yellow with a dot for whether it lands on a keyframe (green) or not (coral); a click seeks
there. **Streams**: video, audio, subtitle and keyframe sections (count, median and
longest GOP). The footer carries the result (`01:41:42  −6:30  0 re-enc`), or the file's
duration, size and container on the Streams tab. The snapping warning stays, as a coral
note above the footer: it is information the design's panel had no room for.

The facts come from one more `ffprobe -show_format -show_streams` in the scan worker
(`cut::parse_facts`, unit-tested on a JSON sample), so entering the mode still never
blocks. A removed clip is now selected by clicking its ghost on the timeline (X restores
it) — that used to be a Segments row.

The playhead is 1px (was 2) with a shorter pin (9px tab + 7px point, was 14 + 10) and a
softer glow, as the board's last edit has it.

Not built: the thumbnails view draws the film ground, not frames (nothing extracts a frame
per chapter yet); no `+` to add a chapter (the model reads chapters from the file and the
export does not write them).

## 2026-10-05 — cut mode's scrub panel rebuilt to the board's variation A

The Cut board changed after it was first built: the transport row and the six
labelled lanes gave way to one scrub panel, "clips on tracks" (variation A of the new
board 1b). Rebuilt to it; the model, the export, the header and the inspector stand.

- **Toolbar** (50px): play, timecode, then icon buttons — set in (blue), set out
  (red), split, cut selection, snap to keyframes — a snap lamp and a zoom slider
  (log scale between "whole clip" and 600 px/s; − / + step it). The buttons carry no
  words, so the hovered one names itself and its key.
- **Rows** (222px, 56px icon gutter): a time ruler, numbered chapter flags, video and
  audio as one rounded CLIP per kept piece with a grip at each end, a dashed ghost
  per cut, subtitle cues as thin bars, keyframe ticks (the two the selection snapped
  to stand tall). Short windows drop the chapter and audio rows. The overview lane,
  the legend and the long-GOP bracket are gone with the old board.
- **Grips work**: dragging a clip's edge resizes the cut beside it, slides a split, or
  trims a new cut in from the clip's own start/end — nearest keyframe, one undo step
  per drag, never across a neighbour (`Cut::move_edge`). The canvas follows the edge.
- **Razor**: over the clips the pointer shows where `S` will split (snapped), and `S`
  splits there; elsewhere it still splits at the playhead.
- In/out are a blue and a red bar with a tab; the playhead is white with a pin and a
  timecode chip. The pin's point is `Item::TriangleDown` (shader mode 9, `p1 = 2`).
- Icons are built from rects and triangles (`draw_icon`) — the renderer has no paths.
  Chapter titles and cue text, which the board no longer prints, show on hover.

## 2026-10-03 — cut mode (the Timeline Edit design's "Cut" board)

`T` (or the new CUT tab, or `--cut [in,out[,in,out…]]`) puts the focused clip on a
timeline: the Claude Design canvas "Abner Timeline Edit", board 1. Same shell as
Compare with the source rail traded for a 300px inspector on the right (Selection,
Segments, Result) and a lanes panel under the transport (rebuilt 2026-10-05, above).

`src/cut.rs` is the model: removed ranges over SOURCE time (nothing shifts), splits,
an undo stack, and the snap. `I`/`O` set in/out; with the default keyframe snap the
in point moves BACK and the out point FORWARD to keyframes, so every kept range starts
on one and `E` exports `<name>.cut.<ext>` as a stream copy (concat demuxer,
`inpoint`/`outpoint`). `K` switches to frame snap, which re-encodes (select filters,
H.264 CRF 16 + AAC) and says so in the header. `X` cuts the selection (or restores
it when it names an existing cut — click a segment row to select one), `S` splits,
⌘Z undoes, ← → step a frame, ⇧← ⇧→ a keyframe. Playback jumps over cuts.

Keyframes and chapters come from `ffprobe` (packet flags — nothing decoded), the
waveform from a streaming `ffmpeg` decode to 20 peaks/s, subtitles from the first
text stream as SRT; each on its own worker, drained in `tick`.

Departures from the board, on purpose: the warning reads "snapping also cuts N s you
meant to keep" (the board's "keeps … you meant to cut" has it backwards for an out
point that snaps forward); the Keyframes/Subtitles/Chapters tabs are not built, so
they are not drawn; the video lane has no thumbnails and there is no scene detection
(so no "detected scene" marker); cut rows are dimmed and tagged, not struck through
(the renderer has no strike-through and text is never measured app-side). The design's
translucent whites are baked to opaque colours over their known ground (`mix`) —
blending here is linear.

Also fixed: `launch_frame`'s logo-only `FrameDesc` was missing `thumbs` (the tree did
not compile at 9bb5b61).

## 2026-09-30 — recent files on the launch window

The Splash floor carries the last four clips opened, as the Design canvas "abner —
Splash, recent files" draws them: 132×74 rounded (8px) 16:9 thumbnails 64px apart,
a white hairline bevel lit from the top, the container (from the extension) and
`HH:MM:SS` duration on corner chips, `W×H` and fps underneath, under a `RECENT` /
`⌘1–n` header. Click or ⌘1–4 opens a tile through the drop path (so it is recorded
again and moves to the front); with clips up ⌘-digit stays the bare digit. Hover
turns the bevel brand blue and shows a pointer.

`src/recent.rs`: `~/.config/abner/recent`, one absolute (not canonical — no disk
access, no `/tmp` vs `/private/tmp` doubles) path per line, 12 kept, written
atomically by the RUNNER on every load (CLI or drop), never by `App`, so tests can't
touch it. No cache, per the project rule: each launch window re-probes up to 8
candidates and pulls one 320×180 cover-cropped frame at 10% in with an `ffmpeg` child
per file, on worker threads under the probe's deadline; a missing or unreadable file
drops out and the next candidate fills its place, pending tiles hold their slot as
an empty well. Thumbnails live in one 1280×180 atlas bound at slot 8 of every group
(keyless, like the plate), drawn by shader mode 14 with mode 0's rounded-box SDF;
`App` tracks which path each cell holds, so a cell uploads only when it changes.
Only the plate layout gets the row (it needs the floor between horizon and foot).

## 2026-09-28 — config file

`abner.default.toml` (compiled in) holds every setting; the first of `--config`,
`./abner.toml`, `~/.config/abner/abner.toml`, `~/.config/abner.toml` overlays it
by deep table merge. Strict: unknown keys, bad types and out-of-range values refuse
startup with the file and dotted key. Covers window size, starting view, start
paused, seek step, delta gain, blend, checker size and brush size (`src/config.rs`).

## 2026-09-28 — native comparison and editing workspace

Implemented the approved Superdesign workspace in the native renderer: left-aligned
Compare/Mask/Crop tools, colored numbered source rows with metadata, focused
inspector, neutral comparison controls, charcoal canvas, persistent transport and
compact status footer. Replaced the giant letter and overlay panels. All controls
use the existing comparison, brush, crop and export actions; `C` opens crop directly.

Drawing and pointer routing share the workspace geometry. Pan/zoom, frame-locked
comparison, painting and crop exports retain a single image transform; GPU scissors
keep zoomed images and mask overlays inside each canvas cell. Text truncation uses
actual font advances. Responsive layout narrows the source rail below 900px and
hides the inspector below 600px high; `Tab` gives the image the full window.

Validation: all 30 tests pass, including workspace input routing, clip scrolling,
side-by-side zoom anchoring, text fitting, GPU scissor bounds, sync and crop exports.
The native comparison workspace was checked with a targeted window capture.

## 2026-09-28 — `E` exports the crop as ProRes

With the marquee up, `E` re-encodes the focused clip — every frame, audio as PCM —
cut to the marquee, as ProRes 422 Proxy beside the source: `clip.mp4` →
`clip.crop.mov` (ProRes lives in QuickTime, so `.mov` whatever the source was).
It shells out to `ffmpeg` on a worker (`mask::export_prores`) rather than going
through the in-process decoder: it's an offline transcode, and ffmpeg's autorotate
puts the crop filter in the same display-pixel space as the marquee. Written to a
temporary and renamed on success, like the PNGs. The rectangle is the same one `S`
cuts, floored to even sides (`mask::even_rect`) because 4:2:2 needs whole chroma
pairs; the origin doesn't move. Progress/result ride the status line through their
own receiver (`crop_export`), so a PNG save never queues behind a long encode.

## 2026-09-27 — skip the splash for command-line launches

Any arguments disable the animated launch splash and skip probing its video asset.
Video paths go straight to playback; `--no-video-splash` alone opens a static,
centered logo without the plate, reflection, status lamp, or footer. Drag-and-drop
still loads clips, and closing the last clip returns to that same logo-only window.
Launching without arguments keeps the existing animated splash.

## 2026-09-23 — the launch window becomes the Splash surface

The launch window now stands on the brand's grid-floor plate instead of a flat
clear: `assets/banner/background-02.png` baked in beside the wordmark (a bare
binary still has no resource directory), cover-fitted and slid until its lit
horizon sits at 57.8% of the window, with the mark standing on that line and its
reflection running out across the floor. This is the design system's `Splash`
surface (Abner > Components > Splash); the geometry notes there and the constants
in `app.rs` are the same numbers.

The horizon is MEASURED, not assumed: `decode_plate` takes the plate's brightest
row and hands `App` its v, the same rule the wordmark's ink box and the glyph
metrics already follow — a different render in the slot moves the layout with it,
and `launch_frame` reads back where the cover fit actually left the line rather
than trusting the fraction it asked for (the clamp that stops the fit opening a
gap can move it).

Three renderer modes, all keyless like the wordmark (the plate is bound at slot 7
of every group): **10** draws the plate, **11** projects the mark onto the floor,
**12** is its contact shadow. Mode 11 inverts a pinhole projection of a plane
hinged at the horizon and tilted 72° — a screen row becomes a distance along the
ground, which is the same hyperbola that makes the grid converge. A straight
vertical flip is a mirror on glass; this is light on a floor. It samples an
explicit LOD (non-uniform control flow forbids derivatives there, and the
softening toward the viewer is wanted anyway), fades in from the horizon as well
as out toward the viewer, and breaks into bands at the plate's own 3px pitch.

Two shipped mistakes, both found in a window capture and worth keeping written
down: at full strength the reflection's first row is barely foreshortened, so it
landed as a second copy of the tagline directly under the real one (hence the
fade-IN); and a contact shadow dropped a twentieth of the mark's height does the
same thing, because the tagline's shadow clears the tagline. The drop is now
1.4%, and the shadow is the one place the brand book allows the lockup a shadow —
the plate's lit horizon runs straight behind the wordmark and leaves the metal
nothing to sit against.

`RectItem` gained `fade_down`, the mirror of `fade_up`: the plate needs a ground
under the traffic lights as well as one under its message. Under 900×520 the
window falls back to the bare mark on the flat clear — below that the vanishing
point leaves the frame and the horizon stops reading, which is the Splash card's
own rule. The message, the drag-hover accent swap and the launch window's alpha
are unchanged; the plate is drawn AT that alpha, so the desktop still shows
faintly through.

## 2026-09-20 — 19. per-video mask painting and grayscale export

M pauses and opens a mask over the focused video, temporarily showing it full-window.
A native-resolution binary raster supplies one 50%-alpha red/blue overlay: painted
blue replaces untouched red. Circular strokes sweep between pointer events, so quick
drags have no gaps. The brush is measured in image pixels; the overlay, pointer
outline, and hit mapping share the existing zoom/pan transform. +/- changes diameter,
Enter switches per-video masks, and M/Esc returns to the previous comparison view.

S snapshots the focused mask and saves `<video-stem>.mask.png` on a worker thread,
with white painted pixels and black untouched pixels. PNG encoding finishes before
an atomic sibling-file replacement; failures are visible in the mask strip. No new
dependencies. `--mask` makes the state directly reachable for window-capture checks.

Regression tests cover continuous/clipped strokes, exact grayscale PNG bytes and
polarity, zoom mapping, letterbox/status hit rejection, independent video masks,
focused export, and drop/replacement lifetime; all 18 tests pass. The release bundle
builds and passes codesign verification. A live targeted-window drag confirmed the
continuous blue stroke and brush ring, and S produced a verified 1280×720 grayscale
PNG with the save confirmation visible in the HUD. Clippy reports pre-existing lints.

## 2026-09-16 — Open With / double-click on the .app (TASKS.md 3)

Opening two mp4s on the installed bundle produced AppKit's own dialog — *"Abner cannot
open files in the “MPEG-4 movie” format"* — because the plist declared no document types
and nothing answered the odoc Apple Event: LaunchServices delivers opened files to the
application delegate, never as argv, so without a handler the default
`NSDocumentController` answers for the app.

**Fix.** switchblade's `open.rs` + `open_shim.m`, ported as `src/open.rs` +
`src/open_shim.m` (compiled by a new `build.rs` via `cc`). winit 0.30 owns the
`NSApplicationDelegate` and panics if it is replaced, so the shim grafts
`application:openURLs:` onto winit's delegate CLASS with `class_addMethod`, right after
`EventLoop::new` and before the run loop starts (AppKit checks `respondsToSelector` at
`finishLaunching`, and the cold-launch open event fires then). Paths buffer in a static
and `about_to_wait` drains them into `Runner::files_dropped(paths, false)` — the drop
path already does the right thing, and it is always an ADD, never the ⌘-replace, since
a ⌘ held while picking a menu item must not wipe the set. The player `Notify` hook wakes
the loop, so an open into an idling app redraws promptly.

`packaging/Info.plist.in` now carries `CFBundleDocumentTypes` (mp4/m4v, mov, mkv, webm,
avi, mpeg/ts, wmv, flv, 3gp and `public.movie`) plus `UTImportedTypeDeclarations` for
the containers macOS has no UTI for, using the identifiers VLC/IINA already import so
LaunchServices unifies rather than forks the type. No `CFBundleTypeIconFile` — the app
icon stands in. Verified with `open -a /Applications/Abner.app a.mp4 b.mp4` (title
`a.mp4 vs b.mp4`, both slots filled) and a second `open` of `c.mp4` into the running
instance (same pid, title gains `vs c.mp4`).

## 2026-09-06 — build against the keg-only ffmpeg@8

A clean `cargo build --release` stopped compiling, with five errors reported inside
`rsmpeg 0.18.0+ffmpeg.8.0`'s own source — `attempted to take value of method pix_fmts`,
`supported_framerates`, `sample_fmts`, and an `AVCodecID` expected `u32`, found `i32`.
Nothing in this repo had changed.

**Cause.** Homebrew's `ffmpeg` moved to 9.0.1 (libavcodec 63). rusty_ffmpeg generates its
bindings with bindgen from whatever headers pkg-config finds, so the build silently
retargeted ffmpeg 9, where the `AVCodec` array fields are accessor functions and
`AVCodecID` changed signedness. rsmpeg's source is written against the 8.x layout, so the
mismatch surfaces as a broken dependency crate rather than as "wrong ffmpeg" — which is
the whole trap: the errors point at a file nobody here wrote. 0.18 is rsmpeg's newest
release; there is no ffmpeg 9 support upstream to upgrade to.

**Fix.** `brew install ffmpeg@8` (8.1.2, keg-only, so it doesn't unlink ffmpeg 9) and a
new `.cargo/config.toml` that puts its pkgconfig dir on `PKG_CONFIG_PATH` for both
Homebrew prefixes. Not `force`d, so an explicit `PKG_CONFIG_PATH` still wins, and
pkg-config ignores the prefix that doesn't exist. A bare `cargo build --release` works
again with no shell setup — which matters, because the failure mode is confusing enough
that a required export would eventually be forgotten.

The two ffmpegs coexist deliberately. Decoding links the 8.x dylibs (the bundle now
carries `libavcodec.62`, and `build-app.sh` picks that up unchanged since it walks
`otool -L`); the startup probe shells out to `ffprobe` from PATH, which is ffmpeg 9 and
parses identically. Suite green, both sync tests included: the test clips are generated
by the ffmpeg 9 CLI and decoded by the ffmpeg 8 libs.

## 2026-09-05 — 18. app icon conforms to the macOS 26 icon guideline

The Dock drew the app icon shrunk inside a lighter rounded plate, visibly smaller than
every neighbouring icon, and re-centring or trimming the PNG changed nothing on screen.

**Cause, and it is documented.** Apple documents this outright (HIG > App icons > Icon shape, rev. 2026-06-08): *"Produce appropriately shaped, unmasked layers. The system masks all layer edges to produce an icon's final shape. For iOS, iPadOS, and macOS icons, provide square layers so the system can apply rounded corners. Providing layers with pre-defined masking negatively impacts specular highlight effects and makes edges look jagged."* and *"If you do import a background layer, make sure it's full-bleed and opaque."* macOS 26 applies the mask itself, so an app
icon must be **square, full-bleed, opaque and unmasked at 1024²**. The renders in
`assets/icons/` are none of those: a rounded body floating in a transparent margin at ~89%
of its canvas, carrying its own bevel and glow (the guideline separately says to avoid soft
feathered edges and to leave highlights, bevels and glows to the system). Not meeting the
spec, it was adapted by the system rather than masked by it — hence the plate.

**Fix.** `scripts/trim-icon.py` trims to the alpha bounding box and scales the artwork up
until the 1024² centre crop is opaque corner to corner, then flattens to RGB. `--zoom`
defaults to the smallest value that achieves this, found by binary search — 1.198 for this
render, keeping 83% — so the crop is never tighter than the guideline requires. The
render's own rounded corners are cropped away and the system's mask supplies the
silhouette. `assets/app-icon.png` is regenerated through it.

Switchblade's slot icon already satisfied the guideline (1254², fully opaque, edge to
edge), which is why its Dock icon was always right on the same `CFBundleIconFile`-only
bundle layout. Its `assets/variants/` alternates do not — `make-variants.sh` seats a
pre-masked superellipse at `BODY=824` on a 1024 canvas — and are filed as a task there.

**The process failure is the point.** Two confident causes were written into this file and
then disproven, and a third was reached by building throwaway .app bundles and measuring
what `NSWorkspace.icon(forFile:)` composited: synthetic squircles at 824 vs 1024, crisp vs
soft alpha, cream vs magenta. A wrong turn before that had blamed `CFBundleIconName` /
`Assets.car` and would have cost a full Xcode install and an Icon Composer redraw. All of
it was one paragraph of published guideline. **Read the platform's own documentation
before building a test rig against the platform.** The one artefact worth keeping is
switchblade's `packaging/check-icon.swift`, which renders what the OS actually composites
for a built bundle — useful for verifying a change, not for deriving the rule.

## 2026-09-05 — logo on the launch window, palette from the logo, transparent window

The launch window's wordmark is now `assets/logo.png` instead of tracked-out "ABNER"
type. The image already carries the "VIDEO QUALITY TESTING TOOLKIT" line, so the
second text run went with it.

- The renderer owns the mark, the way it owns the font: `include_bytes!` +
  the `png` crate (the Dock icon is decoded by AppKit, but a TEXTURE needs the pixels
  in-process, and nothing else in the tree can produce them), into a mipped
  `Rgba8UnormSrgb` texture — it is drawn at ~460 logical px from a 2056px source, so
  the mip chain is doing real work. Bound at slot 5 of EVERY bind group, so shader
  mode 7 rides whatever batch is current and needs no key. And it MEASURES the file:
  `decode_logo` takes the alpha bounding box and hands `App` the trimmed aspect plus
  the uv rect to draw, because logo.png carries ~12% margin at the top and ~18% at the
  bottom and drawn whole the mark sits visibly high in its own box. App-side code
  never guesses the proportions — the rule the text stack already follows.
- **Palette re-cut from the mark.** 2a's lime (#a6e22e) read as a different product
  sitting under the logo, so `LIME*` became `ACCENT*` = the mark's upper bar (#006dcf)
  and `ACCENT_B` = its lower bar (#e71b24, used as-is). The blue is the one deliberate
  departure from the file: at #006dcf it clears only ~4:1 against the HUD's black,
  which is under the bar for 10–11px mono, so the accent ships lifted to #1580de —
  next to the mark it still reads as the same blue, and illegible status text would
  not have. The launch window gives slot A the blue and slot B the red — the pair on
  screen is the pair on the mark
  directly above it. Both hues are far more saturated than lime, so the zone washes
  were re-weighted DOWN (0.05 → 0.035): blending is linear-space, and 5% of a primary
  already reads as a solid coloured panel. The "which slot fills next" affordance the
  original got from lime-vs-white is kept explicitly — the next free zone is the
  bright one, later empty zones sit at a twelfth of the fill and a fifth of the
  border.
- **The window is slightly transparent.** `with_transparent(true)`, a premultiplied
  surface alpha mode (falling back through post-multiplied to opaque), and
  `FrameDesc::clear` widened to RGBA — the clear colour is scaled by its own alpha at
  the `LoadOp`, since the blend state accumulates premultiplied. Backgrounds run at
  0.92 (frame) and 0.90 (launch). Video quads write alpha 1, so the picture itself is
  never see-through; only the app background, the letterbox and the launch plate let
  the desktop through. Verified from the window capture's own alpha channel
  (background pixel `07 07 09 e7`), not by eye — a capture composited on white looks
  identical to an opaque window.

## 2026-09-04 — no visible titlebar

Switchblade's glass titlebar: `setTitlebarAppearsTransparent` +
`NSWindowTitleHidden` + `FullSizeContentView`, so the wgpu surface fills the strip and
the frame runs edge to edge. The transparent bar on its own would show the default
system grey rather than the app's clear — the content view has to extend under it for
the strip to match. Traffic lights kept (they float over the content), so top-anchored
HUD rows — corner brackets, the A|B pill, the info block, the launch window's
brackets — are offset by `App::top_inset()`: `TITLEBAR_H` (28) windowed, 0 in fake
fullscreen, where the window is borderless. The window title is still set, only hidden.

## 2026-09-04 — drop-to-load (TASKS.md 1)

Drag clips onto the window and they load. Slots fill in drop order: one file fills
**A** and the launch window stays up half filled, two land as **A**/**B** and start
playing, more add C, D…. A drop onto a running comparison ADDS streams; ⌘ held at drop
time replaces the whole set. `abner one.mp4` now opens the same half-filled window
instead of erroring out.

- Event plumbing is switchblade's `FilesDropped` path (`sb-window/src/lib.rs`): winit
  reports one `DroppedFile` per file with no end-of-batch marker, so they accumulate in
  `window_event` and flush as ONE gesture from `about_to_wait` — without the batch,
  dropping a pair would land as two separate one-file loads. ⌘ is read from the
  hardware modifier state (`os_primary_modifier_down`), because a drag from Finder
  never focuses this window and so never sends a `ModifiersChanged`.
- `App::add_videos` rewinds EVERY stream to 0 on a load. A clip that kept its position
  while the arrivals decoded from the top would be silently unsynced — the one thing
  this product must never show. `App::ready()` (two or more streams) now gates the
  launch window, the transport hit-testing and the keymap.
- `Gpu::set_video_dims` rebuilds the per-video textures at runtime. Slots whose
  dimensions are unchanged KEEP their texture, so appending a C doesn't blank A and B
  until their next frame lands; any real change drops the pair-bind-group cache, whose
  entries hold views into those textures.
- Probing on the event loop is safe because `probe::run_deadlined` already bounds it —
  a dropped file on a dead mount fails in bounded time instead of hanging the window.
  A file that fails to probe or spawn is logged and skipped, not fatal: a drop is a
  guess by definition.
- The 2b launch window's targets are live state now: a filled slot shows the clip's
  name, `● WxH · codec` and a solid lime border, and a drag over the window brightens
  the empty ones (winit reports no drop POSITION, so both light together and the file
  fills the next free slot).
- New test `dropped_clips_fill_slots_in_order_and_stay_synced`: 0 → 1 (not ready) → 2
  (plays) → 3 (appends), asserting the clock rewinds and the three streams' shown pts
  stay inside a frame period.

## 2026-09-04 — .app bundling (TASKS.md 2)

- `packaging/build-app.sh` + `packaging/Info.plist.in`, switchblade's recipe minus
  document icons and asset folders: bundled ffmpeg dylibs rewritten to `@rpath`,
  ad-hoc codesign, PATH-fixing launcher, LaunchServices refresh, `--install`/`--open`/
  `--with-cli-tools`/`--sign`/`--debug`. Verified: the bundle runs with an empty PATH
  (dylibs + ffprobe lookup), and `open Abner.app` shows the launch window. The
  `cargo-bundle` metadata went — it can't rewrite dylib paths, so its bundle only ran
  on the machine that built it.
- `assets/app-icon.png` is now the icon slot for both the `.icns` and the bare
  binary's Dock icon. `set_app_icon()` returns early inside a bundle (the
  `setApplicationIconImage`-overrides-the-plist trap).
- `scripts/window-id.swift` matches the bundled app's owner name too.

## 2026-09-04 — switchblade catch-up

Assessment of abner against switchblade (forked 2026-07-23; switchblade gained ~166
commits since, of which only a handful touched shared code). Landed the fixes that were
real or latent defects; the rest became TASKS.md items 1–11.

- `0069bc8` docs: README, CLAUDE.md and `--help` brought in line with the code (no more
  drag-and-drop promise, Esc semantics, `--view` names, test list, bundle-icon trap);
  `scripts/window-id.swift` added for targeted window captures.
- `5fe8f57` `cargo update` (~60 transitive bumps, naga 30.0.1; no manifest changes —
  every direct dependency was already on its latest stable) and CLAUDE.md notes.
- `69a8a0c` `src/schedule.rs` lifted verbatim from sb-window. The redraw-cadence rules
  in `about_to_wait` were already a byte-for-byte inline of it; the point is the five
  tests pinning the occlusion / `MIN_FRAME` / idle-deadline invariants. Zero behaviour
  change.
- `1020d0b` text atlas: no mid-frame reset (earlier text in the frame had already baked
  UVs — switchblade's garbled-badge bug), a full atlas refuses + memoizes and wipes at
  `begin_frame()`; per-rect glyph uploads instead of two whole-atlas uploads per dirty
  frame (one of them dead code); 1024² → 2048².
- `380e9dd` decoder robustness from switchblade's post-fork fixes: `Drop` serialises
  with the reader's park loop (lost-wakeup thread leak); AVIO interrupt callback
  installed before `avformat_open_input` so a drop reaches a reader wedged in libav
  I/O on a dead mount (new FIFO-dribble test); a failed seek fails the player instead
  of silently desyncing; the startup `ffprobe` runs under a 30s deadline.

Deliberately NOT ported: per-player pacing anchors (abner's master clock makes the
whole class of bug impossible), the VT hardware scale chain (abner never scales), the
gamma-space surface flip (see TASKS.md 17), and the split text/tile pipeline (abner's
single pipeline cannot express the layering bugs it caused). abner is not joining
switchblade's workspace: the shared code is a few hundred lines that diverged on
purpose, and sb-window's `App` trait is shaped around a tile grid.

## 2026-07-27

- `797ef8f` feat(macos): embed and set Dock icon at startup — PNG `include_bytes!`'d
  and handed to `NSApp.setApplicationIconImage`, since a bare Mach-O has no
  `CFBundleIconFile` and running from a shell is the common case.

## 2026-07-23 — initial build

- `9097359` Window 2a, the Instrument HUD, from the Claude Design mockups: corner
  brackets, A|B toggle, per-clip info block, hover-revealed transport with seek bar,
  keycap legend.
- `fc93185` Launch window for the no-arguments / .app case (2b; drop targets
  decorative — TASKS.md 1).
- `741dcb5` Initial commit: N-player frame-locked sync on one master clock (no
  per-player pacing), exact-seek framestep with pts adoption, overlay / side-by-side /
  delta / split / checker / blend views, photo-style synced zoom, speed control, fake
  fullscreen, mip-chained video textures, ab_glyph text. Adapted from switchblade's
  media and render stack as of that date.
