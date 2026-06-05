#version 450
// Colored-quad vertex shader for Eclipse's View-tree draw pass.
// Input: position already in Vulkan NDC (x,y in [-1,1], y down), and an RGBA color.
layout(location = 0) in vec2 inPos;
layout(location = 1) in vec4 inColor;
layout(location = 0) out vec4 fragColor;
void main() {
    gl_Position = vec4(inPos, 0.0, 1.0);
    fragColor = inColor;
}
