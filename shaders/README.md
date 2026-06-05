# Eclipse shaders

2026-06-05: SPIR-V for the View-tree colored-quad draw pass (`src/graphics.rs`).

The compiled `.spv` files are **committed** and `include_bytes!`-embedded so the build needs
**no shader compiler and no network** (portability — AGENTS.md §9, builds from a clean checkout on
any machine). The `.glsl`-style `.vert`/`.frag` sources are kept alongside so the SPIR-V is
regenerable and auditable.

To regenerate after editing a shader (requires a Vulkan SDK `glslangValidator` on the dev machine
only — not on end-user or CI build machines):

```sh
glslangValidator -V shaders/quad.vert -o shaders/quad.vert.spv
glslangValidator -V shaders/quad.frag -o shaders/quad.frag.spv
```

`quad.vert` / `quad.frag` are a trivial position+color pipeline: the vertex shader takes a `vec2`
position already in Vulkan NDC plus an RGBA color and passes the color through; the fragment shader
outputs the interpolated color. Used to draw each recorded View's screen rect as a flat quad.

`text.vert` / `text.frag` are the R8 glyph-atlas pipeline (sample a single-channel coverage atlas,
tint with a push-constant color). `composite.vert` / `composite.frag` are the RGBA Canvas-composite
pipeline (2026-06-05): a custom View's `onDraw(Canvas)` rasterizes into an RGBA8 `Pixmap` (tiny-skia);
the fragment shader samples that texture and scales its alpha by a push-constant opacity, alpha-blended
over the view quads + text. Regenerate the same way:

```sh
glslangValidator -V shaders/text.vert -o shaders/text.vert.spv
glslangValidator -V shaders/text.frag -o shaders/text.frag.spv
glslangValidator -V shaders/composite.vert -o shaders/composite.vert.spv
glslangValidator -V shaders/composite.frag -o shaders/composite.frag.spv
```
