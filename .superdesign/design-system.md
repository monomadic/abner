# Abner video workspace
An open-ended video editing tool, halfway between a player and an editor. NOT a conventional timeline editor. Viewing, comparing, masking, cropping, and exporting form a fluid workspace around the image. Playback is supporting infrastructure, not the product hierarchy.

Native macOS Rust/wgpu app, one window. Video pixels remain central and synchronized; zoom, pan and all comparison modes remain intact. Black canvas, restrained #1580de blue focus, #e71b24 mask mode, system monospace type. Compact panels, consistent spacing, quiet outlines. Native traffic lights and borderless title strip. Preserve branded launch artwork.

Refresh the loaded workspace: compact source list; focused source inspector; discoverable Compare / Mask / Crop actions using existing capabilities; small clip identifiers; calm transport and shortcut hints. No timeline, invented filters, tracks, media library, or nonfunctional feature buttons. Preserve direct clip switching, frame steps, mask paint, crop and export.

## Workspace annotation refinements
Number clips 1–9 to match their keyboard shortcuts, including inspector and canvas identity. Keep each number independently legible; color supplements the number. Current clips: 1 sky #61b5ee, 2 amber #e6ac69, 3 violet #b89ae8. Show filename, resolution, fps, codec and duration within every clip row; remove the SOURCES heading. Remove the View heading. Comparison options use quiet borderless controls: inactive radius 6px, selected radius 7px, selected surface #1a1a1a and text #d1d1d1. The video canvas surround uses #1d1b1b at every viewport; this does not recolor the footage or other panels. Negative annotated border width means 0px.
