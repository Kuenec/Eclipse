#version 450
// Textured-glyph fragment shader: sample the R8 coverage atlas as alpha, tint with a push-constant
// color. Premultiply-free straight alpha (blended by the pipeline).
layout(location = 0) in vec2 fragUv;
layout(location = 0) out vec4 outColor;
layout(set = 0, binding = 0) uniform sampler2D atlas;
layout(push_constant) uniform PushColor { vec4 color; } pc;
void main() {
    float coverage = texture(atlas, fragUv).r;
    outColor = vec4(pc.color.rgb, pc.color.a * coverage);
}
