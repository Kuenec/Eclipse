#version 450

layout(location = 0) in vec2 fragUv;
layout(location = 0) out vec4 outColor;
layout(set = 0, binding = 0) uniform sampler2D atlas;
layout(push_constant) uniform PushColor { vec4 color; } pc;
void main() {
    float coverage = texture(atlas, fragUv).r;
    outColor = vec4(pc.color.rgb, pc.color.a * coverage);
}
