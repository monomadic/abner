# Player dependencies
- src/main.rs (window, input)
  - src/app.rs (layout, modes, interaction)
    - src/render.rs (rect, text, video primitives)
      - src/text.rs (font measurements and glyph atlas)
      - src/shader.wgsl (drawing)
    - src/mask.rs (mask and crop)
    - src/player.rs (decoding)
    - src/probe.rs (metadata)
