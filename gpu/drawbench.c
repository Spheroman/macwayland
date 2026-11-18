// Frames per second, offscreen (docs/gpu.md, step 3): the same source
// for Linux (under macwl-run, through the render node) and macOS
// (KosmicKrisp directly), to compare.
//
//   drawbench draws FRAMES DRAWS   draw-bound: DRAWS tiny triangles a
//                                  frame, each with its push constants
//   drawbench fill FRAMES PASSES   GPU-bound: PASSES full-screen
//                                  triangles of a heavy fragment shader
//
// 1920x1080 RGBA8, three frames in flight, command buffers recorded anew
// each frame (as an app does; Venus's per-draw cost is in recording).
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <vulkan/vulkan.h>

#include "draw.vert.h"
#include "draw.frag.h"

#define CHECK(x) do { VkResult r_ = (x); if (r_ != VK_SUCCESS) { fprintf(stderr, "%s: %d\n", #x, r_); exit(1); } } while (0)
#define W 1920
#define H 1080
#define IN_FLIGHT 3

struct push { float x, y, scale, work; };

static double now_s(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec + ts.tv_nsec / 1e9;
}

int main(int argc, char **argv) {
    if (argc < 4) {
        fprintf(stderr, "usage: drawbench draws|fill FRAMES COUNT\n");
        return 2;
    }
    int fill = !strcmp(argv[1], "fill");
    int frames = atoi(argv[2]), count = atoi(argv[3]);

    VkApplicationInfo app = {.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO, .apiVersion = VK_API_VERSION_1_1};
    VkInstanceCreateInfo ici = {.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo = &app};
#ifdef __APPLE__
    const char *exts[] = {"VK_KHR_portability_enumeration"};
    ici.flags = VK_INSTANCE_CREATE_ENUMERATE_PORTABILITY_BIT_KHR;
    ici.enabledExtensionCount = 1;
    ici.ppEnabledExtensionNames = exts;
#endif
    VkInstance inst;
    CHECK(vkCreateInstance(&ici, NULL, &inst));
    uint32_t n = 1;
    VkPhysicalDevice phys;
    VkResult er = vkEnumeratePhysicalDevices(inst, &n, &phys);
    if ((er != VK_SUCCESS && er != VK_INCOMPLETE) || !n) {
        fprintf(stderr, "no GPU\n");
        return 1;
    }
    VkPhysicalDeviceProperties props;
    vkGetPhysicalDeviceProperties(phys, &props);
    float prio = 1;
    VkDeviceQueueCreateInfo qci = {.sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, .queueCount = 1, .pQueuePriorities = &prio};
    VkDeviceCreateInfo dci = {.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, .queueCreateInfoCount = 1, .pQueueCreateInfos = &qci};
    VkDevice dev;
    CHECK(vkCreateDevice(phys, &dci, NULL, &dev));
    VkQueue q;
    vkGetDeviceQueue(dev, 0, 0, &q);

    // The target: an optimal-tiling image in device-local memory.
    VkImageCreateInfo ii = {.sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO, .imageType = VK_IMAGE_TYPE_2D, .format = VK_FORMAT_R8G8B8A8_UNORM,
                            .extent = {W, H, 1}, .mipLevels = 1, .arrayLayers = 1, .samples = VK_SAMPLE_COUNT_1_BIT,
                            .tiling = VK_IMAGE_TILING_OPTIMAL, .usage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT};
    VkImage img;
    CHECK(vkCreateImage(dev, &ii, NULL, &img));
    VkMemoryRequirements req;
    vkGetImageMemoryRequirements(dev, img, &req);
    VkPhysicalDeviceMemoryProperties mp;
    vkGetPhysicalDeviceMemoryProperties(phys, &mp);
    uint32_t type = 0;
    while (type < mp.memoryTypeCount && !(((req.memoryTypeBits >> type) & 1) && (mp.memoryTypes[type].propertyFlags & VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT))) type++;
    VkMemoryAllocateInfo mai = {.sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .allocationSize = req.size, .memoryTypeIndex = type};
    VkDeviceMemory mem;
    CHECK(vkAllocateMemory(dev, &mai, NULL, &mem));
    CHECK(vkBindImageMemory(dev, img, mem, 0));
    VkImageViewCreateInfo vci = {.sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO, .image = img, .viewType = VK_IMAGE_VIEW_TYPE_2D, .format = ii.format,
                                 .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1}};
    VkImageView view;
    CHECK(vkCreateImageView(dev, &vci, NULL, &view));

    VkAttachmentDescription att = {.format = ii.format, .samples = VK_SAMPLE_COUNT_1_BIT, .loadOp = VK_ATTACHMENT_LOAD_OP_CLEAR,
                                   .storeOp = VK_ATTACHMENT_STORE_OP_STORE, .stencilLoadOp = VK_ATTACHMENT_LOAD_OP_DONT_CARE,
                                   .stencilStoreOp = VK_ATTACHMENT_STORE_OP_DONT_CARE, .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED,
                                   .finalLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL};
    VkAttachmentReference ref = {0, VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL};
    VkSubpassDescription sub = {.pipelineBindPoint = VK_PIPELINE_BIND_POINT_GRAPHICS, .colorAttachmentCount = 1, .pColorAttachments = &ref};
    VkRenderPassCreateInfo rpi = {.sType = VK_STRUCTURE_TYPE_RENDER_PASS_CREATE_INFO, .attachmentCount = 1, .pAttachments = &att, .subpassCount = 1, .pSubpasses = &sub};
    VkRenderPass rp;
    CHECK(vkCreateRenderPass(dev, &rpi, NULL, &rp));
    VkFramebufferCreateInfo fbi = {.sType = VK_STRUCTURE_TYPE_FRAMEBUFFER_CREATE_INFO, .renderPass = rp, .attachmentCount = 1, .pAttachments = &view, .width = W, .height = H, .layers = 1};
    VkFramebuffer fb;
    CHECK(vkCreateFramebuffer(dev, &fbi, NULL, &fb));

    VkShaderModuleCreateInfo smi = {.sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO, .codeSize = sizeof(draw_vert), .pCode = draw_vert};
    VkShaderModule vs, fs;
    CHECK(vkCreateShaderModule(dev, &smi, NULL, &vs));
    smi.codeSize = sizeof(draw_frag);
    smi.pCode = draw_frag;
    CHECK(vkCreateShaderModule(dev, &smi, NULL, &fs));
    VkPushConstantRange pcr = {VK_SHADER_STAGE_VERTEX_BIT | VK_SHADER_STAGE_FRAGMENT_BIT, 0, sizeof(struct push)};
    VkPipelineLayoutCreateInfo pli = {.sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO, .pushConstantRangeCount = 1, .pPushConstantRanges = &pcr};
    VkPipelineLayout layout;
    CHECK(vkCreatePipelineLayout(dev, &pli, NULL, &layout));
    VkPipelineShaderStageCreateInfo stages[2] = {
        {.sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, .stage = VK_SHADER_STAGE_VERTEX_BIT, .module = vs, .pName = "main"},
        {.sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, .stage = VK_SHADER_STAGE_FRAGMENT_BIT, .module = fs, .pName = "main"},
    };
    VkPipelineVertexInputStateCreateInfo vi = {.sType = VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO};
    VkPipelineInputAssemblyStateCreateInfo ia = {.sType = VK_STRUCTURE_TYPE_PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO, .topology = VK_PRIMITIVE_TOPOLOGY_TRIANGLE_LIST};
    VkViewport vp = {0, 0, W, H, 0, 1};
    VkRect2D sc = {{0, 0}, {W, H}};
    VkPipelineViewportStateCreateInfo vps = {.sType = VK_STRUCTURE_TYPE_PIPELINE_VIEWPORT_STATE_CREATE_INFO, .viewportCount = 1, .pViewports = &vp, .scissorCount = 1, .pScissors = &sc};
    VkPipelineRasterizationStateCreateInfo rs = {.sType = VK_STRUCTURE_TYPE_PIPELINE_RASTERIZATION_STATE_CREATE_INFO, .polygonMode = VK_POLYGON_MODE_FILL, .cullMode = VK_CULL_MODE_NONE, .lineWidth = 1};
    VkPipelineMultisampleStateCreateInfo ms = {.sType = VK_STRUCTURE_TYPE_PIPELINE_MULTISAMPLE_STATE_CREATE_INFO, .rasterizationSamples = VK_SAMPLE_COUNT_1_BIT};
    VkPipelineColorBlendAttachmentState cba = {.colorWriteMask = 0xf};
    VkPipelineColorBlendStateCreateInfo cb = {.sType = VK_STRUCTURE_TYPE_PIPELINE_COLOR_BLEND_STATE_CREATE_INFO, .attachmentCount = 1, .pAttachments = &cba};
    VkGraphicsPipelineCreateInfo gpi = {.sType = VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO, .stageCount = 2, .pStages = stages,
                                        .pVertexInputState = &vi, .pInputAssemblyState = &ia, .pViewportState = &vps, .pRasterizationState = &rs,
                                        .pMultisampleState = &ms, .pColorBlendState = &cb, .layout = layout, .renderPass = rp};
    VkPipeline pipe;
    CHECK(vkCreateGraphicsPipelines(dev, VK_NULL_HANDLE, 1, &gpi, NULL, &pipe));

    VkCommandPool pools[IN_FLIGHT];
    VkCommandBuffer cbs[IN_FLIGHT];
    VkFence fences[IN_FLIGHT];
    for (int i = 0; i < IN_FLIGHT; i++) {
        VkCommandPoolCreateInfo pci = {.sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO, .flags = VK_COMMAND_POOL_CREATE_TRANSIENT_BIT};
        CHECK(vkCreateCommandPool(dev, &pci, NULL, &pools[i]));
        VkCommandBufferAllocateInfo cai = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO, .commandPool = pools[i], .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount = 1};
        CHECK(vkAllocateCommandBuffers(dev, &cai, &cbs[i]));
        VkFenceCreateInfo fci = {.sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO, .flags = VK_FENCE_CREATE_SIGNALED_BIT};
        CHECK(vkCreateFence(dev, &fci, NULL, &fences[i]));
    }

    double t0 = 0;
    const int warm = 10;
    for (int f = 0; f < frames + warm; f++) {
        if (f == warm)
            t0 = now_s();
        int i = f % IN_FLIGHT;
        CHECK(vkWaitForFences(dev, 1, &fences[i], VK_TRUE, UINT64_MAX));
        CHECK(vkResetFences(dev, 1, &fences[i]));
        CHECK(vkResetCommandPool(dev, pools[i], 0));
        VkCommandBufferBeginInfo bi = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO, .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT};
        CHECK(vkBeginCommandBuffer(cbs[i], &bi));
        VkClearValue clear = {.color = {{0, 0, 0, 1}}};
        VkRenderPassBeginInfo rbi = {.sType = VK_STRUCTURE_TYPE_RENDER_PASS_BEGIN_INFO, .renderPass = rp, .framebuffer = fb, .renderArea = sc, .clearValueCount = 1, .pClearValues = &clear};
        vkCmdBeginRenderPass(cbs[i], &rbi, VK_SUBPASS_CONTENTS_INLINE);
        vkCmdBindPipeline(cbs[i], VK_PIPELINE_BIND_POINT_GRAPHICS, pipe);
        for (int d = 0; d < count; d++) {
            struct push p = fill ? (struct push){0, 0, 1, 64} : (struct push){(d % 100) / 50.0f - 1, (d / 100 % 100) / 50.0f - 1, 0.004f, 0};
            vkCmdPushConstants(cbs[i], layout, pcr.stageFlags, 0, sizeof p, &p);
            vkCmdDraw(cbs[i], 3, 1, 0, 0);
        }
        vkCmdEndRenderPass(cbs[i]);
        CHECK(vkEndCommandBuffer(cbs[i]));
        VkSubmitInfo si = {.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = 1, .pCommandBuffers = &cbs[i]};
        CHECK(vkQueueSubmit(q, 1, &si, fences[i]));
    }
    CHECK(vkWaitForFences(dev, IN_FLIGHT, fences, VK_TRUE, UINT64_MAX));
    double t = now_s() - t0;
    printf("%s: %s %d a frame, %d frames: %.1f fps (%.2f ms a frame)\n", props.deviceName, fill ? "full-screen passes" : "draws", count, frames,
           frames / t, t * 1e3 / frames);
    return 0;
}
