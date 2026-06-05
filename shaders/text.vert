#version 450
// Textured-glyph vertex shader for Eclipse's View-tree text pass.
// Input: position already in Vulkan NDC, plus atlas UV. Color is a push constant (text color).
layout(location = 0) in vec2 inPos;
layout(location = 1) in vec2 inUv;
layout(location = 0) out vec2 fragUv;
void main() {
    gl_Position = vec4(inPos, 0.0, 1.0);
    fragUv = inUv;
}
