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
