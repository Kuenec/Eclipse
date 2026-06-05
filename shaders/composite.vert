#version 450
// Canvas-composite vertex shader: a custom View's onDraw rasterized into an RGBA Pixmap, drawn as a
// textured quad over the view's screen rect. Input: position already in Vulkan NDC, plus texture UV.
layout(location = 0) in vec2 inPos;
layout(location = 1) in vec2 inUv;
layout(location = 0) out vec2 fragUv;
void main() {
    gl_Position = vec4(inPos, 0.0, 1.0);
    fragUv = inUv;
}
