// A Vulkan Wayland client that draws only when a key comes (docs/gpu.md,
// step 4): a window cleared to one colour, then, for each key pressed, the
// next colour, presented at once. Under macwl-shell's MACWL_AUTO_KEYS it
// measures a guest GPU client's key → glass latency, as foot does for a
// software one.
//
//   keyframe [fifo|mailbox]     present mode (default fifo)
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#include <wayland-client.h>
#define VK_USE_PLATFORM_WAYLAND_KHR
#include <vulkan/vulkan.h>

#include "xdg-shell-client-protocol.h"

#define CHECK(x) do { VkResult r_ = (x); if (r_ != VK_SUCCESS) { fprintf(stderr, "%s: %d\n", #x, r_); exit(1); } } while (0)

static struct wl_compositor *compositor;
static struct xdg_wm_base *wm_base;
static struct wl_seat *seat;
static int width = 640, height = 480, configured, resized, keys, quit;
// KEYFRAME_TRACE=1: when each key came and each stage of its answer (µs on
// CLOCK_MONOTONIC: the host's uptime clock under macwl-run).
static int trace;
static double key_at;
static double now_us(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1e6 + ts.tv_nsec / 1e3;
}

static void ping(void *d, struct xdg_wm_base *b, uint32_t serial) { (void)d; xdg_wm_base_pong(b, serial); }
static const struct xdg_wm_base_listener wm_listener = {ping};

static void surface_configure(void *d, struct xdg_surface *s, uint32_t serial) {
    (void)d;
    xdg_surface_ack_configure(s, serial);
    configured = 1;
}
static const struct xdg_surface_listener surface_listener = {surface_configure};

static void toplevel_configure(void *d, struct xdg_toplevel *t, int32_t w, int32_t h, struct wl_array *st) {
    (void)d, (void)t, (void)st;
    if (w > 0 && h > 0 && (w != width || h != height)) {
        width = w;
        height = h;
        resized = 1;
    }
}
static void toplevel_close(void *d, struct xdg_toplevel *t) { (void)d, (void)t; quit = 1; }
static void toplevel_bounds(void *d, struct xdg_toplevel *t, int32_t w, int32_t h) { (void)d, (void)t, (void)w, (void)h; }
static void toplevel_caps(void *d, struct xdg_toplevel *t, struct wl_array *c) { (void)d, (void)t, (void)c; }
static const struct xdg_toplevel_listener toplevel_listener = {toplevel_configure, toplevel_close, toplevel_bounds, toplevel_caps};

static void kb_keymap(void *d, struct wl_keyboard *k, uint32_t f, int32_t fd, uint32_t s) { (void)d, (void)k, (void)f, (void)s; close(fd); }
static void kb_enter(void *d, struct wl_keyboard *k, uint32_t s, struct wl_surface *sf, struct wl_array *a) { (void)d, (void)k, (void)s, (void)sf, (void)a; }
static void kb_leave(void *d, struct wl_keyboard *k, uint32_t s, struct wl_surface *sf) { (void)d, (void)k, (void)s, (void)sf; }
static void kb_key(void *d, struct wl_keyboard *k, uint32_t s, uint32_t t, uint32_t key, uint32_t state) {
    (void)d, (void)k, (void)s, (void)t;
    if (state == WL_KEYBOARD_KEY_STATE_PRESSED) {
        if (key == 1) // Esc
            quit = 1;
        keys++;
        key_at = now_us();
    }
}
static void kb_mods(void *d, struct wl_keyboard *k, uint32_t s, uint32_t a, uint32_t b, uint32_t c, uint32_t g) { (void)d, (void)k, (void)s, (void)a, (void)b, (void)c, (void)g; }
static void kb_repeat(void *d, struct wl_keyboard *k, int32_t r, int32_t de) { (void)d, (void)k, (void)r, (void)de; }
static const struct wl_keyboard_listener kb_listener = {kb_keymap, kb_enter, kb_leave, kb_key, kb_mods, kb_repeat};

static void global(void *d, struct wl_registry *r, uint32_t name, const char *iface, uint32_t v) {
    (void)d, (void)v;
    if (!strcmp(iface, wl_compositor_interface.name))
        compositor = wl_registry_bind(r, name, &wl_compositor_interface, 4);
    else if (!strcmp(iface, xdg_wm_base_interface.name)) {
        wm_base = wl_registry_bind(r, name, &xdg_wm_base_interface, 1);
        xdg_wm_base_add_listener(wm_base, &wm_listener, NULL);
    } else if (!strcmp(iface, wl_seat_interface.name))
        seat = wl_registry_bind(r, name, &wl_seat_interface, 1);
}
static void global_remove(void *d, struct wl_registry *r, uint32_t n) { (void)d, (void)r, (void)n; }
static const struct wl_registry_listener registry_listener = {global, global_remove};

static VkDevice dev;
static VkPhysicalDevice phys;
static VkSurfaceKHR vk_surface;
static VkSwapchainKHR swapchain;
static VkImage images[8];
static uint32_t image_count;
static VkPresentModeKHR mode = VK_PRESENT_MODE_FIFO_KHR;

static void make_swapchain(void) {
    VkSurfaceCapabilitiesKHR caps;
    CHECK(vkGetPhysicalDeviceSurfaceCapabilitiesKHR(phys, vk_surface, &caps));
    uint32_t n = 16;
    VkSurfaceFormatKHR formats[16];
    CHECK(vkGetPhysicalDeviceSurfaceFormatsKHR(phys, vk_surface, &n, formats));
    VkSurfaceFormatKHR f = formats[0];
    for (uint32_t i = 0; i < n; i++)
        if (formats[i].format == VK_FORMAT_B8G8R8A8_UNORM)
            f = formats[i];
    VkSwapchainKHR old = swapchain;
    VkSwapchainCreateInfoKHR sci = {
        .sType = VK_STRUCTURE_TYPE_SWAPCHAIN_CREATE_INFO_KHR,
        .surface = vk_surface,
        .minImageCount = caps.minImageCount + 1 > 3 ? caps.minImageCount + 1 : 3,
        .imageFormat = f.format,
        .imageColorSpace = f.colorSpace,
        .imageExtent = {width, height},
        .imageArrayLayers = 1,
        .imageUsage = VK_IMAGE_USAGE_TRANSFER_DST_BIT | VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT,
        .preTransform = VK_SURFACE_TRANSFORM_IDENTITY_BIT_KHR,
        .compositeAlpha = VK_COMPOSITE_ALPHA_OPAQUE_BIT_KHR,
        .presentMode = mode,
        .clipped = VK_TRUE,
        .oldSwapchain = old,
    };
    CHECK(vkCreateSwapchainKHR(dev, &sci, NULL, &swapchain));
    if (old)
        vkDestroySwapchainKHR(dev, old, NULL);
    image_count = 8;
    CHECK(vkGetSwapchainImagesKHR(dev, swapchain, &image_count, images));
}

int main(int argc, char **argv) {
    trace = getenv("KEYFRAME_TRACE") != NULL;
    if (argc > 1 && !strcmp(argv[1], "mailbox"))
        mode = VK_PRESENT_MODE_MAILBOX_KHR;
    struct wl_display *display = wl_display_connect(NULL);
    if (!display) {
        fprintf(stderr, "no Wayland display\n");
        return 1;
    }
    struct wl_registry *registry = wl_display_get_registry(display);
    wl_registry_add_listener(registry, &registry_listener, NULL);
    wl_display_roundtrip(display);
    if (!compositor || !wm_base || !seat) {
        fprintf(stderr, "missing globals\n");
        return 1;
    }
    struct wl_keyboard *kb = wl_seat_get_keyboard(seat);
    wl_keyboard_add_listener(kb, &kb_listener, NULL);
    struct wl_surface *surface = wl_compositor_create_surface(compositor);
    struct xdg_surface *xs = xdg_wm_base_get_xdg_surface(wm_base, surface);
    xdg_surface_add_listener(xs, &surface_listener, NULL);
    struct xdg_toplevel *top = xdg_surface_get_toplevel(xs);
    xdg_toplevel_add_listener(top, &toplevel_listener, NULL);
    xdg_toplevel_set_title(top, "keyframe");
    wl_surface_commit(surface);
    while (!configured)
        wl_display_dispatch(display);

    const char *iexts[] = {VK_KHR_SURFACE_EXTENSION_NAME, VK_KHR_WAYLAND_SURFACE_EXTENSION_NAME};
    VkApplicationInfo app = {.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO, .apiVersion = VK_API_VERSION_1_1};
    VkInstanceCreateInfo ici = {.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo = &app, .enabledExtensionCount = 2, .ppEnabledExtensionNames = iexts};
    VkInstance inst;
    CHECK(vkCreateInstance(&ici, NULL, &inst));
    uint32_t n = 1;
    VkResult er = vkEnumeratePhysicalDevices(inst, &n, &phys);
    if ((er != VK_SUCCESS && er != VK_INCOMPLETE) || !n) {
        fprintf(stderr, "no GPU\n");
        return 1;
    }
    VkWaylandSurfaceCreateInfoKHR wci = {.sType = VK_STRUCTURE_TYPE_WAYLAND_SURFACE_CREATE_INFO_KHR, .display = display, .surface = surface};
    CHECK(vkCreateWaylandSurfaceKHR(inst, &wci, NULL, &vk_surface));
    float prio = 1;
    VkDeviceQueueCreateInfo qci = {.sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, .queueCount = 1, .pQueuePriorities = &prio};
    const char *dexts[] = {VK_KHR_SWAPCHAIN_EXTENSION_NAME};
    VkDeviceCreateInfo dci = {.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, .queueCreateInfoCount = 1, .pQueueCreateInfos = &qci, .enabledExtensionCount = 1, .ppEnabledExtensionNames = dexts};
    CHECK(vkCreateDevice(phys, &dci, NULL, &dev));
    VkQueue q;
    vkGetDeviceQueue(dev, 0, 0, &q);
    make_swapchain();

    VkCommandPoolCreateInfo pci = {.sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO, .flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT};
    VkCommandPool pool;
    CHECK(vkCreateCommandPool(dev, &pci, NULL, &pool));
    VkCommandBufferAllocateInfo cai = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO, .commandPool = pool, .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount = 1};
    VkCommandBuffer cb;
    CHECK(vkAllocateCommandBuffers(dev, &cai, &cb));
    VkSemaphoreCreateInfo semi = {.sType = VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO};
    VkSemaphore acquired, rendered;
    CHECK(vkCreateSemaphore(dev, &semi, NULL, &acquired));
    CHECK(vkCreateSemaphore(dev, &semi, NULL, &rendered));
    VkFenceCreateInfo fci = {.sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO, .flags = VK_FENCE_CREATE_SIGNALED_BIT};
    VkFence done;
    CHECK(vkCreateFence(dev, &fci, NULL, &done));

    static const VkClearColorValue colours[] = {{{0.1f, 0.1f, 0.1f, 1}}, {{0.9f, 0.3f, 0.2f, 1}}, {{0.2f, 0.7f, 0.3f, 1}}, {{0.2f, 0.4f, 0.9f, 1}}};
    int drawn = -1;
    while (!quit) {
        if (resized) {
            resized = 0;
            CHECK(vkDeviceWaitIdle(dev));
            make_swapchain();
            drawn = -1;
        }
        if (drawn != keys) {
            drawn = keys;
            double t0 = now_us();
            CHECK(vkWaitForFences(dev, 1, &done, VK_TRUE, UINT64_MAX));
            CHECK(vkResetFences(dev, 1, &done));
            uint32_t i;
            double t1 = now_us();
            CHECK(vkAcquireNextImageKHR(dev, swapchain, UINT64_MAX, acquired, VK_NULL_HANDLE, &i));
            double t2 = now_us();
            VkCommandBufferBeginInfo bi = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO, .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT};
            CHECK(vkBeginCommandBuffer(cb, &bi));
            VkImageSubresourceRange range = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1};
            VkImageMemoryBarrier to_dst = {.sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER, .dstAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT,
                                           .oldLayout = VK_IMAGE_LAYOUT_UNDEFINED, .newLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
                                           .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED, .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED, .image = images[i], .subresourceRange = range};
            vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 0, NULL, 0, NULL, 1, &to_dst);
            vkCmdClearColorImage(cb, images[i], VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, &colours[keys % 4], 1, &range);
            VkImageMemoryBarrier to_present = to_dst;
            to_present.srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT;
            to_present.dstAccessMask = 0;
            to_present.oldLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL;
            to_present.newLayout = VK_IMAGE_LAYOUT_PRESENT_SRC_KHR;
            vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT, 0, 0, NULL, 0, NULL, 1, &to_present);
            CHECK(vkEndCommandBuffer(cb));
            VkPipelineStageFlags wait_stage = VK_PIPELINE_STAGE_TRANSFER_BIT;
            VkSubmitInfo si = {.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .waitSemaphoreCount = 1, .pWaitSemaphores = &acquired, .pWaitDstStageMask = &wait_stage,
                               .commandBufferCount = 1, .pCommandBuffers = &cb, .signalSemaphoreCount = 1, .pSignalSemaphores = &rendered};
            CHECK(vkQueueSubmit(q, 1, &si, done));
            double t3 = now_us();
            VkPresentInfoKHR pi = {.sType = VK_STRUCTURE_TYPE_PRESENT_INFO_KHR, .waitSemaphoreCount = 1, .pWaitSemaphores = &rendered, .swapchainCount = 1, .pSwapchains = &swapchain, .pImageIndices = &i};
            VkResult pr = vkQueuePresentKHR(q, &pi);
            if (pr == VK_ERROR_OUT_OF_DATE_KHR || pr == VK_SUBOPTIMAL_KHR)
                resized = 1;
            else
                CHECK(pr);
            if (trace)
                fprintf(stderr, "keyframe: key %.1f loop +%.0f fence +%.0f acquire +%.0f record+submit +%.0f present +%.0f\n", key_at,
                        t0 - key_at, t1 - t0, t2 - t1, t3 - t2, now_us() - t3);
        }
        // Then wait for the next key (or configure).
        if (wl_display_dispatch(display) < 0)
            break;
    }
    vkDeviceWaitIdle(dev);
    return 0;
}
