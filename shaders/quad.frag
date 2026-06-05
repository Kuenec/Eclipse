#version 450
// Colored-quad fragment shader: output the interpolated vertex color.
layout(location = 0) in vec4 fragColor;
layout(location = 0) out vec4 outColor;
void main() {
    outColor = fragColor;
}
