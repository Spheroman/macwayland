#version 450
// A small triangle at the push constant's offset (draw-bound) or a
// full-screen one (GPU-bound: scale 3).
layout(push_constant) uniform P { vec2 offset; float scale; float work; } p;
void main() {
    vec2 v[3] = vec2[](vec2(-1, -1), vec2(3, -1), vec2(-1, 3));
    gl_Position = vec4(p.offset + v[gl_VertexIndex] * p.scale, 0, 1);
}
