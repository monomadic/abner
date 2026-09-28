# Abner — video workspace refresh

Abner is an open-ended video editing tool between a player and an editor. The workspace centers on direct operations on video, without a track timeline.

## Proposed hierarchy

1. Sources: a compact, collapsible rail for A, B, C and further clips. Each row shows identity and duration; selecting it focuses the source. Full technical details belong in one inspector for the focused source.
2. Operations: Compare, Mask and Crop remain visible above the image. Their settings and available output actions appear in a contextual row. Export is contextual to the selected crop, matching current capabilities.
3. Canvas: the largest region. Small source identifiers replace dominant letters. Keep synchronized zoom, pan, frame stepping and comparison views. Interface hiding restores an unobstructed view.
4. Transport: one quiet seek control and frame/time readout, supporting the work without occupying the main hierarchy.
5. Status: concise mode and operation feedback; shortcut hints supplement visible controls.

## Interaction constraints

- Compare supports single, side-by-side, difference, split/wipe, checker and blend; retain existing parameters.
- Enter/number keys continue to select sources. Sidebar selection should do the same.
- Mask retains image-pixel brush sizing and per-source masks. Tool controls must never become paint targets.
- Crop operates on the same image coordinates as mask painting. Save still + mask and export ProRes use the existing behavior.
- The central canvas rectangle must be shared by rendering and pointer conversion. Repositioning the picture cannot introduce a second zoom transform.
- Source filenames must be measured and truncated in the renderer. Do not position adjacent labels using guessed text widths.
- No timeline, tracks, effect browser, project persistence, undo, or media-bin behavior is implied by this visual refresh.

## Delivery status

Visual proposal only. Native Rust UI has not been changed by this task. Concurrent edits were observed in app.rs and text.rs while the initial code was being inspected. Baseline test attempt intersected an incomplete edit and failed on temporarily missing INFO_W / INFO_CH constants; that is not a verification of the settled tree.
