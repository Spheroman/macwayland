#version 450
// `work` iterations of arithmetic per pixel (GPU-bound), else a colour.
layout(push_constant) uniform P { vec2 offset; float scale; float work; } p;
layout(location = 0) out vec4 color;
void main() {
    vec2 q = gl_FragCoord.xy * 0.001;
    float a = 0.0;
    for (int i = 0; i < int(p.work); i++)
        a = fract(a * 1.37 + sin(q.x * float(i) + q.y));
    color = vec4(a, q, 1);
}
