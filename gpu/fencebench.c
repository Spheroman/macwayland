// Fence round trips (docs/gpu.md, step 2): submit a command buffer with a
// fence and wait for it, N times; print the round trip's p50, p90 and p99
// in µs. The same source builds for Linux (run under macwl-run, through
// the render node) and natively for macOS (KosmicKrisp directly), to
// compare. `fencebench [N] [fill-bytes]`: the command buffer fills a
// buffer of that many bytes (0: empty). `fencebench N copy W H host|device`:
// it clears a WxH image and copies it into a buffer (as Mesa's WSI does
// for a frame on its prime path), the buffer in host-visible or
// device-local memory.
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <vulkan/vulkan.h>

#define CHECK(x) do { VkResult r_ = (x); if (r_ != VK_SUCCESS) { fprintf(stderr, "%s: %d\n", #x, r_); exit(1); } } while (0)

static double now_us(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1e6 + ts.tv_nsec / 1e3;
}

static int cmp(const void *a, const void *b) {
    double x = *(const double *)a, y = *(const double *)b;
    return x < y ? -1 : x > y;
}

int main(int argc, char **argv) {
    int n = argc > 1 ? atoi(argv[1]) : 2000;
    int copy = argc > 2 && !strcmp(argv[2], "copy");
    VkDeviceSize fill = argc > 2 && !copy ? strtoull(argv[2], NULL, 0) : 0;
    uint32_t cw = copy && argc > 3 ? atoi(argv[3]) : 1920, ch = copy && argc > 4 ? atoi(argv[4]) : 1200;
    int host_mem = !(copy && argc > 5 && !strcmp(argv[5], "device"));

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
    uint32_t count = 1;
    VkPhysicalDevice phys;
    VkResult er = vkEnumeratePhysicalDevices(inst, &count, &phys);
    if ((er != VK_SUCCESS && er != VK_INCOMPLETE) || count == 0) {
        fprintf(stderr, "no GPU\n");
        return 1;
    }
    VkPhysicalDeviceProperties props;
    vkGetPhysicalDeviceProperties(phys, &props);

    float prio = 1;
    VkDeviceQueueCreateInfo qci = {.sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, .queueFamilyIndex = 0, .queueCount = 1, .pQueuePriorities = &prio};
    VkDeviceCreateInfo dci = {.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, .queueCreateInfoCount = 1, .pQueueCreateInfos = &qci};
    VkDevice dev;
    CHECK(vkCreateDevice(phys, &dci, NULL, &dev));
    VkQueue q;
    vkGetDeviceQueue(dev, 0, 0, &q);

    VkCommandPoolCreateInfo pci = {.sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO, .queueFamilyIndex = 0};
    VkCommandPool pool;
    CHECK(vkCreateCommandPool(dev, &pci, NULL, &pool));
    VkCommandBufferAllocateInfo cai = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO, .commandPool = pool, .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount = 1};
    VkCommandBuffer cb;
    CHECK(vkAllocateCommandBuffers(dev, &cai, &cb));

    VkBuffer buf = VK_NULL_HANDLE;
    if (fill) {
        VkBufferCreateInfo bci = {.sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, .size = fill, .usage = VK_BUFFER_USAGE_TRANSFER_DST_BIT};
        CHECK(vkCreateBuffer(dev, &bci, NULL, &buf));
        VkMemoryRequirements req;
        vkGetBufferMemoryRequirements(dev, buf, &req);
        VkPhysicalDeviceMemoryProperties mp;
        vkGetPhysicalDeviceMemoryProperties(phys, &mp);
        uint32_t type = 0;
        while (type < mp.memoryTypeCount && !((req.memoryTypeBits >> type) & 1)) type++;
        VkMemoryAllocateInfo mai = {.sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .allocationSize = req.size, .memoryTypeIndex = type};
        VkDeviceMemory mem;
        CHECK(vkAllocateMemory(dev, &mai, NULL, &mem));
        CHECK(vkBindBufferMemory(dev, buf, mem, 0));
    }
    // GPU timestamps around the work: how long the GPU itself took.
    VkQueryPoolCreateInfo qpi = {.sType = VK_STRUCTURE_TYPE_QUERY_POOL_CREATE_INFO, .queryType = VK_QUERY_TYPE_TIMESTAMP, .queryCount = 2};
    VkQueryPool qp;
    CHECK(vkCreateQueryPool(dev, &qpi, NULL, &qp));
    VkCommandBufferBeginInfo bi = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO};
    CHECK(vkBeginCommandBuffer(cb, &bi));
    vkCmdResetQueryPool(cb, qp, 0, 2);
    vkCmdWriteTimestamp(cb, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, qp, 0);
    if (fill)
        vkCmdFillBuffer(cb, buf, 0, VK_WHOLE_SIZE, 0x12345678);
    if (copy) {
        VkPhysicalDeviceMemoryProperties mp;
        vkGetPhysicalDeviceMemoryProperties(phys, &mp);
        VkImageCreateInfo ii = {.sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO, .imageType = VK_IMAGE_TYPE_2D, .format = VK_FORMAT_B8G8R8A8_UNORM,
                                .extent = {cw, ch, 1}, .mipLevels = 1, .arrayLayers = 1, .samples = VK_SAMPLE_COUNT_1_BIT, .tiling = VK_IMAGE_TILING_OPTIMAL,
                                .usage = VK_IMAGE_USAGE_TRANSFER_SRC_BIT | VK_IMAGE_USAGE_TRANSFER_DST_BIT};
        VkImage img;
        CHECK(vkCreateImage(dev, &ii, NULL, &img));
        VkMemoryRequirements ir;
        vkGetImageMemoryRequirements(dev, img, &ir);
        uint32_t it = 0;
        while (!((ir.memoryTypeBits >> it) & 1)) it++;
        VkMemoryAllocateInfo imai = {.sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .allocationSize = ir.size, .memoryTypeIndex = it};
        VkDeviceMemory imem;
        CHECK(vkAllocateMemory(dev, &imai, NULL, &imem));
        CHECK(vkBindImageMemory(dev, img, imem, 0));
        // The buffer: a row pitch aligned to 256, as WSI's.
        uint32_t pitch = (cw * 4 + 255) & ~255u;
        VkBufferCreateInfo bci = {.sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, .size = (VkDeviceSize)pitch * ch, .usage = VK_BUFFER_USAGE_TRANSFER_DST_BIT};
        VkBuffer dst;
        CHECK(vkCreateBuffer(dev, &bci, NULL, &dst));
        VkMemoryRequirements br;
        vkGetBufferMemoryRequirements(dev, dst, &br);
        uint32_t bt = 0, want = host_mem ? VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT : VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT;
        while (bt < mp.memoryTypeCount && !(((br.memoryTypeBits >> bt) & 1) && (mp.memoryTypes[bt].propertyFlags & want))) bt++;
        VkMemoryAllocateInfo bmai = {.sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .allocationSize = br.size, .memoryTypeIndex = bt};
        VkDeviceMemory bmem;
        CHECK(vkAllocateMemory(dev, &bmai, NULL, &bmem));
        CHECK(vkBindBufferMemory(dev, dst, bmem, 0));
        VkImageSubresourceRange range = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1};
        VkImageMemoryBarrier b1 = {.sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER, .dstAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT, .oldLayout = VK_IMAGE_LAYOUT_UNDEFINED,
                                   .newLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED, .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
                                   .image = img, .subresourceRange = range};
        vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 0, NULL, 0, NULL, 1, &b1);
        VkClearColorValue c = {{0.9f, 0.3f, 0.2f, 1}};
        vkCmdClearColorImage(cb, img, VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, &c, 1, &range);
        VkImageMemoryBarrier b2 = b1;
        b2.srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT;
        b2.dstAccessMask = VK_ACCESS_TRANSFER_READ_BIT;
        b2.oldLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL;
        b2.newLayout = VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL;
        vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 0, NULL, 0, NULL, 1, &b2);
        VkBufferImageCopy region = {.bufferRowLength = pitch / 4, .imageSubresource = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1}, .imageExtent = {cw, ch, 1}};
        vkCmdCopyImageToBuffer(cb, img, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL, dst, 1, &region);
        fill = (VkDeviceSize)pitch * ch;
    }
    vkCmdWriteTimestamp(cb, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT, qp, 1);
    CHECK(vkEndCommandBuffer(cb));

    VkFenceCreateInfo fci = {.sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO};
    VkFence fence;
    CHECK(vkCreateFence(dev, &fci, NULL, &fence));
    VkSubmitInfo si = {.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = 1, .pCommandBuffers = &cb};

    // FB_IDLE_MS: idle this long before each round (a key's answer after a
    // pause, not a busy loop).
    const char *warm_env = getenv("FB_WARM_US");
    int warm_us = warm_env ? atoi(warm_env) : -1;
    VkCommandBuffer warm_cb;
    VkCommandBufferAllocateInfo wcai = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO, .commandPool = pool, .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount = 1};
    CHECK(vkAllocateCommandBuffers(dev, &wcai, &warm_cb));
    VkCommandBufferBeginInfo wbi = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO};
    CHECK(vkBeginCommandBuffer(warm_cb, &wbi));
    // A real job, however small: an empty one may never reach the GPU.
    {
        VkBufferCreateInfo wbci = {.sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, .size = 4096, .usage = VK_BUFFER_USAGE_TRANSFER_DST_BIT};
        VkBuffer wbuf;
        CHECK(vkCreateBuffer(dev, &wbci, NULL, &wbuf));
        VkMemoryRequirements wr;
        vkGetBufferMemoryRequirements(dev, wbuf, &wr);
        uint32_t wt = 0;
        while (!((wr.memoryTypeBits >> wt) & 1)) wt++;
        VkMemoryAllocateInfo wmai = {.sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .allocationSize = wr.size, .memoryTypeIndex = wt};
        VkDeviceMemory wmem;
        CHECK(vkAllocateMemory(dev, &wmai, NULL, &wmem));
        CHECK(vkBindBufferMemory(dev, wbuf, wmem, 0));
        vkCmdFillBuffer(warm_cb, wbuf, 0, 4, 0);
    }
    CHECK(vkEndCommandBuffer(warm_cb));
    VkSubmitInfo warm_si = {.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = 1, .pCommandBuffers = &warm_cb};
    const char *idle_env = getenv("FB_IDLE_MS");
    int idle_ms = idle_env ? atoi(idle_env) : 0;
    double *t = malloc(sizeof(double) * n);
    double *g = malloc(sizeof(double) * n);
    for (int i = (idle_ms ? -2 : -50); i < n; i++) { // (warm up first)
        if (idle_ms) {
            struct timespec ts = {idle_ms / 1000, (idle_ms % 1000) * 1000000L};
            nanosleep(&ts, NULL);
        }
        // FB_WARM_US: first an empty submission, to wake the GPU, this long
        // before the timed one (what a compositor could do on a key).
        if (warm_us >= 0) {
            CHECK(vkQueueSubmit(q, 1, &warm_si, VK_NULL_HANDLE));
            struct timespec ts = {0, warm_us * 1000L};
            nanosleep(&ts, NULL);
        }
        double t0 = now_us();
        CHECK(vkQueueSubmit(q, 1, &si, fence));
        CHECK(vkWaitForFences(dev, 1, &fence, VK_TRUE, UINT64_MAX));
        double t1 = now_us();
        CHECK(vkResetFences(dev, 1, &fence));
        if (i >= 0) {
            t[i] = t1 - t0;
            uint64_t ts[2] = {0, 0};
            if (vkGetQueryPoolResults(dev, qp, 0, 2, sizeof ts, ts, 8, VK_QUERY_RESULT_64_BIT) == VK_SUCCESS)
                g[i] = (ts[1] - ts[0]) * props.limits.timestampPeriod / 1e3;
            else
                g[i] = -1;
        }
    }
    qsort(t, n, sizeof(double), cmp);
    qsort(g, n, sizeof(double), cmp);
    printf("%s: submit + fence wait, %d rounds, %s %llu bytes: p50 %.1f µs, p90 %.1f µs, p99 %.1f µs\n", props.deviceName, n,
           copy ? (host_mem ? "clear + copy to host-visible" : "clear + copy to device-local") : "fill", (unsigned long long)fill, t[n / 2], t[n * 9 / 10], t[n * 99 / 100]);
    printf("  GPU time: p50 %.1f µs, p90 %.1f µs\n", g[n / 2], g[n * 9 / 10]);
    vkDestroyFence(dev, fence, NULL);
    vkDestroyDevice(dev, NULL);
    vkDestroyInstance(inst, NULL);
    return 0;
}
