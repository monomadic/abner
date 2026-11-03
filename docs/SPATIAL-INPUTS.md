# Spatial input editors

Crop and Mask are temporary editor sessions, launched by an owning control.
Apply returns an immutable value to that owner; Cancel drops the draft. Only one
session is open at a time. Source selection cannot retarget an open draft.
Editing clones mask ownership while sharing immutable bytes until a paint changes
them. Every mask snapshot has a distinct renderer identity.

## Ownership and rendering

- Source crops belong to independent source items; duplicates copy the crop and
  mask values, with independent later edits.
- A newly opened timeline snapshots the source crop. Clip overrides use source
  time intervals, survive splits, and travel with trimmed boundaries. They are
  serialized with the timeline edit and participate in undo. Different crop
  values cannot be joined into one clip.
- Plugin bindings are keyed by stable source identity, processor ID, and input
  key. Source list insertion/reordering cannot redirect another source's binding.
  A job snapshots bindings, including shared immutable mask data. Applying edits
  invalidates pending preview work.

Source and timeline crops fit the selected rectangle into the source-sized canvas
with bilinear sampling and black letterboxing. This maintains a constant output
geometry across clips. The same transformation runs before preview and export
processing. A plugin editor shows that transformed input. Timeline export excludes
removed intervals and starts fresh processor sessions at kept segment boundaries.
The legacy stream-copy exporter refuses pixel crops; use the Export workspace.

## Processor API

Implement `Factory::spatial_inputs()` to declare named controls independently of
numeric `settings_schema` properties. Each declaration has a unique `key`, a
user-facing `title`, a kind, and a required flag:

| Kind | Delivered value |
| --- | --- |
| `Image` | Cropped RGB16 frame with original timing |
| `MaskedImage` | Cropped RGB16 frame plus matching 8-bit coverage |
| `Mask` | Cropped 8-bit coverage and its image-space region |
| `Region` | Image-space rectangle; the primary image stays complete |

The primary `Frame` is always provided separately. Named image inputs do not
black out or erase covered pixels. Coverage is 0 for unselected and 255 for
selected; intermediate values are representable. Masks are authored in the full,
display-oriented input grid and gathered with the exact same crop as the image.

Override `Session::process_with_inputs(frame, inputs)` to consume the values.
The host's `create_bound` wrapper resolves inputs on every processed frame,
including frames released by upstream temporal buffering. Old factories declare
no spatial inputs and continue through their existing `process` implementation.
A missing required binding, missing mask, out-of-bounds crop, or changed input
geometry returns an error rather than guessing a coordinate transform.

`SpatialBinding` carries dimensions, an optional rectangle, an immutable mask,
and a revision. `SpatialInput` carries the input key/kind, image/mask as appropriate,
its rectangle in the primary frame, and the binding revision. Two image inputs can
use different rectangles and only one need have a mask.

## Example and checks

Run `cargo run --example spatial-inputs -- --processor example.spatial clip.mp4`.
The example declares reference image, affected image, and region controls. Paint
the affected-image mask, Apply, enable the example, and render a frame. It tints
only selected samples, demonstrating delivery independently of any external
processor implementation.

Coverage includes cancellation/ownership, inherited crops, split/trim/undo,
legacy timeline decoding, cropped-image/mask alignment, fixed output dimensions,
and interval isolation. Native-window review verified the temporary Crop and Mask
tabs, return to the calling parameter, crop cancellation, and a mask-driven rendered
preview. The full host suite passed with native media access: 182 passed, 3 ignored.
The processor API suite passed both tests. The restricted test environment could
not open the HEVC encoder; the same export test passed with native media access.

## Current limits

Spatial inputs currently require the first enabled processor. Editing arbitrary
intermediate stages needs a stage-input preview; the host rejects that arrangement
rather than authoring a mask against the wrong picture. Coordinate grids must
match exactly. There are no animated masks, keyframed crops, tracking, GPU input
negotiation, or arbitrary secondary media sources in this first implementation.

Source edits and plugin bindings persist for the application session. Timeline
crops persist in the saved timeline edit. Numeric presets exclude image-specific
bindings. PNG export is still available; importing masks and serializing bindings
with a complete project remain follow-up work. Crop is represented in image pixels;
preview/export retain the host's existing SDR/RGB16 and codec restrictions.
