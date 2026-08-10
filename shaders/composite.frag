#version 450



layout(location = 0) in vec2 fragUv;
layout(location = 0) out vec4 outColor;
layout(set = 0, binding = 0) uniform sampler2D canvasTex;
layout(push_constant) uniform PushOpacity { vec4 opacity; } pc;
void main() {
    vec4 texel = texture(canvasTex, fragUv);
    outColor = vec4(texel.rgb, texel.a * pc.opacity.x);
}
