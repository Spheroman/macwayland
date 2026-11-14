# GPU: Vulkan through Venus, GL through Zink

Status: design (2026-11-14). ROADMAP item 11. Nothing built or measured
yet; costs below are estimates from measured ones (`docs/processes.md`,
`docs/mn-threads.md`, `docs/d3dmetal-proton.md`). Worked out with a
second model as design partner, against Mesa main (`c9ce420e`,
2026-11-14) and upstream virglrenderer (`01637fc`, 2026-11-11).

## The goal

Linux apps that draw with Vulkan, GL or GLES do it on the Mac's GPU, and
their frames reach macwl-compositor with latency comparable to a native
CAMetalLayer app. Software rendering (llvmpipe, cairo, WebRender's
software path) is what they get today.

Who needs it, from the targets:

- **GL/GLES**: Firefox and Zen (WebRender), GTK 4's GL renderer, nested
  Hyprland (GLES 3 over EGL, expects GBM and dmabufs), Gazebo's Ogre2 (GL
  3.3 core, or its experimental Vulkan backend).
- **Vulkan**: DXVK and VKD3D-Proton (beside D3DMetal, `docs/d3dmetal-proton.md`,
  which stays a separate path), games, and Zink itself.

## Decisions

- **The guest runs stock Mesa**: Venus for Vulkan, Zink on Venus for GL
  and GLES, Mesa's own WSI, EGL and GBM. Nothing patched in the guest, so
  any distribution's (or nixpkgs') Mesa works under `--root` and on the
  host root.
- **The runtime is a virtio-gpu kernel driver**: `/dev/dri/renderD128`
  with the virtio-gpu DRM uapi (Venus context type, blob resources,
  execbuffer, DRM syncobjs, sync_files, PRIME export, dma-buf sync
  ioctls). No Metal in `macwl-run`, ever.
- **The host side is virglrenderer's Venus renderer on KosmicKrisp**, in a
  helper process of its own (`macwl-gpu`), never in the host.
- **Frames are linear textures over shm**, offered to the compositor as
  `zwp_linux_dmabuf_v1` buffers with the LINEAR modifier, whose fds are
  shm objects: the zero-copy path the shell already has for `wl_shm`
  (`newBufferWithBytesNoCopy`), under the protocol Mesa speaks.
  IOSurfaces only if measuring says so.
- **Venus waits for rendering before it commits**, in its present thread,
  so the compositor needs no fences. No explicit sync protocol.

### Rejected

- **A split Metal driver** (the ROADMAP's first sentence for item 11:
  KosmicKrisp in the guest, its Metal bridge remoted to the host). The
  bridge is small (`src/kosmickrisp/bridge/`, ~170 functions, already
  stubbed on non-Darwin), but it's an internal, unversioned API we'd
  re-sync alone on every Mesa release; it has synchronous returns on
  creation paths (`mtl_buffer_get_gpu_address`,
  `mtl_texture_get_gpu_resource_id`) and host-to-guest callbacks; it does
  nothing for GBM and dmabufs; and every guest root would need our Mesa.
  Where state tracking runs doesn't change latency: guest code runs at
  native speed, and what costs is synchronous round trips, which the
  transport decides. Kept as the fallback if spike 2 shows Venus's
  overhead is the bottleneck. (A community project's numbers, below,
  suggest a remoted thin Metal layer does work.)
- **MoltenVK as the host driver**: no transform feedback (MoltenVK issue
  #1588, open) and no geometry shaders that we could find (not in 1.4.x's
  release notes; steamac reports tessellation plus geometry pipelines
  failing), both of which Zink needs for desktop GL.
- **virgl (GL remoting)**: needs host GL, and the Mac's is 4.1 and
  deprecated.
- **ANGLE split or remoted**: GLES only, and a second remoting project.
- **The host side inside `macwl-run`**: Metal aborts the process on
  misuse, and one host runs a whole session, so one bad guest would end
  every Linux process in it. A Metal-using process also mustn't fork. What
  in-process would have given (waking guest futexes, mapping GPU memory
  into the guest) a helper gets for a hop of ~10 µs, or doesn't need (all
  mappable GPU memory is shm, below).
- **Explicit sync (`wp_linux_drm_syncobj_v1`)**: Venus can't use it
  (`wsi_device_supports_explicit_sync`, `wsi_common.c`, wants
  timeline-semaphore OPAQUE_FD export; Venus exports SYNC_FD only,
  `vn_physical_device.c`), and doesn't need it.

## Shape

```
 Linux app
   │  Vulkan / GL / GLES / EGL / GBM
   ▼
 Mesa in the guest: Venus (+ Zink), WSI
   │  Vulkan commands in Venus's ring (a blob in shm);
   │  virtio-gpu ioctls on /dev/dri/renderD128
   ▼
 macwl-run: the virtio-gpu "kernel": contexts, blobs (shm objects),
   │  syncobjs, sync_files, execbuffer → the helper
   │  a Unix socket per context, the ring and blobs as shm
   ▼
 macwl-gpu (a Mac process, posix_spawn'd per context):
   virglrenderer's Venus renderer → KosmicKrisp → Metal
   │  frames: linear MTLTextures over shm blobs
   ▼
 macwl-compositor (linux-dmabuf, LINEAR over shm) → macwl-shell
```

## Pieces

### Guest: stock Mesa

Venus finds the render node, checks `VIRTGPU_PARAM_*` and the context
type, and runs. Zink loads on top for GL (`MESA_LOADER_DRIVER_OVERRIDE=zink`
if it doesn't pick itself). Mesa's docs still call Zink on macOS
experimental; this is spike 1's main question.

For the first milestone Venus also has a **vtest** transport
(`VN_DEBUG=vtest`, `vn_renderer_vtest.c`): a Unix socket and memfds to
virglrenderer's vtest server. That needs nothing new from the runtime,
but the server needs porting to the Mac: upstream builds `vtest/` only
with vrend, the GL renderer (`meson.build`: `if with_vrend and not
with_host_windows`), and `vtest_shm.c` makes its shm with
`memfd_create`. Not tried.

### `macwl-run`: the virtio-gpu node

A device file in the runtime (as `/dev/ptmx` is), implementing what
Venus and Mesa's DRM WSI use:

- **Params and contexts**: `GETPARAM` (3D features, blob, context init,
  `BLOB_ALIGNMENT` = 16384), `GET_CAPS` (Venus's capset, from the
  helper), `CONTEXT_INIT` with the Venus context type and its rings. A
  context is a helper: the node's first context in a process asks the
  broker for one.
- **Blobs**: the helper allocates the memory, not the runtime: Venus's
  ring and reply shmems (`vkr_context_create_resource_from_shm`,
  `vkr_context.c`) and host-visible device memory at `vkAllocateMemory`
  (`vkr_shm_alloc`, `vkr_device_memory.c`), each an anonymous host file.
  So `RESOURCE_CREATE_BLOB` is a round trip: the helper returns the fd,
  the runtime adopts it as an shm object (`shared.rs` maps it into guest
  physical memory; the compositor can map the same fd) and returns a GEM
  handle; `MAP` gives the offset to `mmap` the node at. These files have
  no shm header, so `shared.rs` takes them as objects of its own
  (`OWN_OBJECT`): futexes in them and windows shared between processes
  need a key for them (the blob's id, say). The other direction, the
  runtime's fd to the helper, is only for imports (`PRIME_FD_TO_HANDLE`,
  `vkr_context_import_resource_from_shm`). Guest-side Venus rounds sizes
  to the reported alignment (`vn_renderer_virtgpu.c`); 16 KiB is the host
  page, so a blob is a whole number of host pages and a window of its
  own.
- **PRIME**: `PRIME_HANDLE_TO_FD` gives the shm fd itself (the "dmabuf");
  `FD_TO_HANDLE` takes one back. Only exported images hold fds; other
  blobs are GEM handles, and the runtime and helper can close their fds
  once mapped.
- **dma-buf sync**: `DMA_BUF_IOCTL_EXPORT_SYNC_FILE` on such an fd returns
  a sync_file already signalled (nothing we know of writes it but the
  GPU, whose writes Venus waits for itself); `IMPORT_SYNC_FILE` accepts it
  and keeps it with the object, for clients that attach dmabufs without
  WSI (below). **Both must succeed**: if either fails,
  `wsi_drm_check_dma_buf_sync_file_import_export` (`wsi_common_drm.c`)
  turns the present semaphore off, nobody waits for rendering, and frames
  tear without a word.
- **Execbuffer**: `EXECBUFFER` with `ring_idx` and `out_syncobjs` (Venus's
  `virtgpu_submit`) passes the command (usually a ring notify) to the
  helper and makes a context fence on that ring; the helper's answer
  retires it.
- **Syncobjs**, with timelines (the 6.6+ uapi: `SYNCOBJ_CREATE`,
  `TIMELINE_WAIT` with `WAIT_FOR_SUBMIT`, `TIMELINE_SIGNAL`, `TRANSFER`,
  `QUERY`, sync_file import and export), and **sync_files** as fds `poll`
  and `epoll` watch (the eventfd machinery on kqueue). With timeline
  syncobjs, `vn_WaitForFences` and `vn_WaitSemaphores` (`vn_sync.c`) block
  in the runtime (`virtgpu_wait` → `timeline_wait`), as a futex does;
  without them Venus falls back on polling sync_files
  (`vn_renderer_sim_syncobj.c`). Real ones from the start.

### `macwl-gpu`: the helper

virglrenderer's render server and its Venus renderer, linked with
KosmicKrisp (Mesa 26.2+, Vulkan 1.4 conformant, macOS 26+ and Metal 4).
Upstream virglrenderer already has what macOS needs:

- workers are `posix_spawn`'d per context, never forked (`render_worker.c`:
  a fork inherits stale XPC connections), which is also the rule here;
- host-visible memory is POSIX shm imported with
  `VK_EXT_external_memory_host` (`vkr_device_memory.c`, `vkr_shm_alloc`);
- the memory layout below (`mem_type_spoof`);
- `VK_KHR_external_memory_fd`, which the guest requires, injected
  (`vkr_physical_device.c`).

How a context gets a helper: virglrenderer's render server is the broker
(started by the session script, listening on a Unix socket, as the D3DMetal
broker does), and it `posix_spawn`s a worker per context, upstream's
model; there's no second spawner of ours. The runtime, not the guest,
connects, at each `CONTEXT_INIT`. A context's socket carries execbuffers
one way and retired fences the other; Venus's own ring (a blob) carries
the Vulkan commands.

If a worker dies (Metal aborts on misuse: the reason it's a process of
its own), the runtime fails the context's ioctls and signals every
pending syncobj point and sync_file of it with an error, so waiters
return (`VK_ERROR_DEVICE_LOST`) rather than hanging until Venus's
watchdog aborts the guest process (it checks the ring's ALIVE bit about
every 3.5 s, `vn_relax`).

Gaps to fill in virglrenderer (patches, upstreamable), since KosmicKrisp
lacks them and the guest's WSI needs them:

| Extension | Without it | Fill |
|---|---|---|
| `VK_EXT_external_memory_dma_buf` | the guest WSI takes its software path (`use_sw_device`, `vn_wsi.c`): a GPU blit and a full-frame copy into `wl_shm` (`WSI_WL_BUFFER_SHM_MEMCPY`, `wsi_common_wayland.c`; 16 MB a frame at 2560×1600, ~1.5–3 ms) | advertised; a "dmabuf" is the blob's shm fd |
| `VK_EXT_image_drm_format_modifier` | the same | LINEAR only: a linear texture over the blob's buffer |
| `VK_EXT_queue_family_foreign` | the same | advertised; ownership transfers are no-ops on one GPU |

(A community project running DXVK over Venus on KosmicKrisp, steamac,
reports patching the LINEAR modifier, shm imported as host memory, 16 KiB
blob alignment and fence signalling: the same list. Unverified.)

### The compositor

`zwp_linux_dmabuf_v1`, advertising the formats the shell can sample with
the LINEAR modifier only; a buffer's single plane is an shm fd, which the
shell maps and draws exactly as a `wl_shm` buffer (wrapped as an
`MTLBuffer`, a linear texture over it). Stride and offset must meet the
GPU's linear-texture alignment; the helper allocates them so. Nothing
else changes: buffers arrive finished.

## Memory

Guest private memory is 4 KiB frames, not contiguous in host memory, so
the GPU can't use it; only shm can be both the guest's and Metal's. That
turns out not to matter: apps reach device memory only through pointers
the driver hands out, and the driver's are shm.

- **Memory types**: KosmicKrisp has one (DEVICE_LOCAL | HOST_VISIBLE, an
  MTLHeap). virglrenderer's spoof splits it as a discrete GPU would: type
  0 DEVICE_LOCAL (an MTLHeap in the helper, never mapped), type 1
  HOST_VISIBLE (shm, imported into Metal with no copy, mapped into the
  guest as a shared window). Same physical memory either way.
- **The hole**: the spoof offers images type 1 too
  (`vkr_physical_device.h`, `(bits << 1) | 1`), but KosmicKrisp's host
  import is for buffers only (`kk_device_memory.c`). Mask type 1 out of
  non-linear images' `memoryTypeBits` (`vkr_image.c`, a few lines).
- **Cost of the split**: texture uploads are GPU copies on any driver
  (optimal-tiled images can't be written through a mapping). What a UMA-
  aware app loses is skipping staging for buffers: here it writes type 1
  directly or pays a memcpy, no GPU work. Not checked: which Zink and DXVK
  pick with this layout.
- **Counts**: one blob per `VkDeviceMemory`, made when first mapped or
  exported (`vn_device_memory.c`); apps suballocate, so tens to hundreds
  per process (estimate), each a shared window (map + unmap 0.5 µs).

## Sync and presenting

Venus owns the wait before commit (`has_implicit_fencing = false`,
`vn_renderer_virtgpu.c`, then set from drirc's `venus_implicit_fencing`,
default false, in `vn_instance.c`; a guest drirc turning it on would
remove the wait): `vkQueuePresentKHR` goes to a present thread
per queue (`vn_wsi_present_async`); common WSI exports the present
semaphore as a sync_file for the dmabuf (`wsi_signal_dma_buf_from_semaphore`),
which Venus waits on (`vn_GetSemaphoreFdKHR` → `vn_wsi_sync_wait`), and
then WSI commits. End to end:

1. The app submits: Venus writes the ring; the helper's ring thread
   decodes and submits to KosmicKrisp; an execbuffer with an out-syncobj
   asks for a fence.
2. Metal finishes; the helper's per-queue sync thread (`vkr_queue.c`,
   `WaitForFences` then `retire_fence`) tells `macwl-run` on its socket.
3. The runtime signals the syncobj point; the present thread's wait
   returns.
4. WSI commits; the compositor draws a finished buffer.

GL clients present the same way: Zink's kopper presents through Venus's
WSI, so they get the same wait (not checked in Zink's source yet).

Clients that attach dmabufs without WSI (nested Hyprland through GBM,
Firefox's textures between its processes) get no such wait, as Venus on
Linux doesn't either. If one tears, the runtime has the fence it imported
for the object (`IMPORT_SYNC_FILE`) and the compositor can be made to
wait on it.

## Costs (estimates, at 120 Hz)

| | |
|---|---|
| Recording and submitting | ring writes, no round trips; decoding runs on another core |
| Calls that wait for the host (creation, queries) | 6.4 µs each (the D3DMetal ring's measured round trip); Venus caches most |
| The host ring asleep | +71 µs once (measured KICK); its idle timeout is 1 ms (`VN_RING_IDLE_TIMEOUT_NS`), so at 120 fps a frame's first command pays it |
| Fence to waiter | 15–25 µs: Metal's completion → sync thread → socket → runtime → guest task |
| Commit | 20–50 µs: a few system calls (0.73 µs each) and the compositor's wake |
| Compositing | one GPU pass, as for `wl_shm` today (the shell matches native: 5.3–6.9 ms p50 against 6.2) |

Against a native CAMetalLayer app: ~0.1–0.3 ms of CPU and under 1 ms of
GPU. The real cost is phase: a native app presents inside its command
buffer, here a frame is finished, committed and then composited, so a
frame that would have made vblank by under ~1 ms slips one. Expect p50 as
native and p90 worse by a fraction of a frame. If spike 3 says that
matters: a client's buffer as the shell's layer contents for a full-screen
client (no composite pass), or GPU-side ordering across processes with
`MTLSharedEvent`.

Venus still polls in a few places (`vn_relax`, `vn_common.c`: 255 yields
then sleeps from 160 µs, doubling, while waiting for a ring reply; 15
yields for ring space, legacy semaphores and queries with WAIT). Replies
come in 6.4 µs, well inside the yields. Our `sched_yield` returns at once
(`linux-abi`: `Ok(0)`), so a spin costs 0.73 µs a yield, but it also never
gives the CPU to another task: with more tasks than CPUs a spinning
Venus thread holds a vCPU for up to ~190 µs. `sched_yield` should yield to
a ready task (`docs/mn-threads.md`'s scheduler has the queue), but only
when one is waiting and no CPU is idle, so the usual reply wait stays
0.73 µs a spin; once it yields, a reply wait can stretch to the other
task's slice (3 ms base), as on Linux: step 3 measures it. The helper
spins too: a thread per ring for 1 ms after each burst (`vkr_ring_relax`,
`vkr_ring.c`), a core's worth while a client draws.

Shader compiles are the same whatever the design: steamac reports 50–100
ms per pipeline the first time, ~1 ms from Metal's cache.

## Risks

1. Zink over Venus over KosmicKrisp: which GL version, and how correct.
   Decides Firefox, GTK 4 and Gazebo.
2. virglrenderer's macOS support: young; its dmabuf assumptions; the vtest
   server isn't built without vrend and needs `memfd_create` (port it, or
   if that's more than a day, skip to the render server and a minimal
   node).
3. The memory layout: the image hole, and Metal with many NoCopy buffers.
4. Linear textures as render targets on a tile-based GPU (no lossless
   compression); a blit to linear may be faster.
5. Pacing: FIFO presents against frame callbacks; `wp_presentation`,
   `fifo-v1` and `commit-timing` in the compositor.
6. macOS 27 and Metal 4 churn (KosmicKrisp tracks workarounds per beta).

## Steps

1. **Does it stand up (vtest)?** Stock nixpkgs Mesa with `VN_DEBUG=vtest`
   under macwl-run, against virglrenderer's vtest server on KosmicKrisp,
   natively on the Mac (ported first: see Risks, 2). Presenting is the software path
   (copies); fine for now.
   Pass: `vulkaninfo` says Vulkan 1.3 or later; a dEQP-VK smoke and
   pipeline subset passes at 99% or better; Zink says GL 3.3 core and GLES
   3.0 or better; a GLES client's pixels are right in the headless
   compositor; an image bound to type 1 works or is masked out.
   Fail: blob memory needs what KosmicKrisp can't do, or Zink is far
   short: prototype the split driver on KosmicKrisp's bridge stubs.
2. **The virtio-gpu node** in the runtime: contexts, blobs, execbuffer,
   timeline syncobjs, sync_files, PRIME, the dma-buf sync ioctls; the
   broker and `macwl-gpu`.
   Pass: Mesa logs no failed sync-file export; a deliberately slow frame
   never shows half drawn in headless dumps; `vkWaitForFences` wakes in
   under 30 µs p50 with no `nanosleep` under `MACWL_STRACE`; killing the
   worker mid-frame gets the guest `VK_ERROR_DEVICE_LOST` within a frame,
   not a hang.
3. **Overhead**: one benchmark (vkoverhead or a 10,000-draw scene, and one
   fill-bound scene) through the guest and natively on KosmicKrisp.
   Pass: 85% of native or better draw-bound, 95% GPU-bound; fence wake p50
   under 30 µs, p99 under 150 µs; no system calls per frame but present's.
   Fail: the case for the split driver.
4. **Zero-copy presenting**: the three extensions in virglrenderer,
   linux-dmabuf in the compositor. Key press to colour change from a guest
   Vulkan app on the `presentedTime` harness, against `native-probe metal
   --no-capture`.
   Pass: p50 within 1 ms and p90 within 2 ms of native (6.2 / 10.5 ms).
   Compare rendering to linear, a blit to linear, and an IOSurface as the
   layer's contents.
   Fail: IOSurfaces over a Mach-port channel to the shell, and
   `MTLSharedEvent` ordering.
5. **Apps**: GTK 4's GL renderer, Firefox with WebRender on GL, Gazebo
   (GL, then `--render-engine-api-backend vulkan`), DXVK.

## Sources

- KosmicKrisp: Mesa's docs (`docs/drivers/kosmickrisp.rst`, and its
  workarounds page); Khronos, "KosmicKrisp achieves Vulkan 1.4
  conformance on Apple silicon"; Phoronix (July and October 2026; the
  October piece read only as a search snippet).
- MoltenVK: KhronosGroup/MoltenVK issue #1588 (transform feedback).
- steamac (github.com/fxgl/steamac, wiki): DXVK → Venus → virglrenderer →
  KosmicKrisp under libkrun on an M1 Max; its numbers (DXVK over Venus
  133–139 fps against 115 for a remoted Metal layer at 2000 draws, 44–50
  against 100 fill-bound) are one source, not checked.
- Mesa source: `src/virtio/vulkan` (`vn_renderer_virtgpu.c`, `vn_wsi.c`,
  `vn_sync.c`, `vn_common.c`, `vn_device_memory.c`, `vn_ring.c`),
  `src/vulkan/wsi` (`wsi_common.c`, `wsi_common_drm.c`,
  `wsi_common_wayland.c`), `src/kosmickrisp`.
- virglrenderer source: `server/render_worker.c`, `src/venus/`
  (`vkr_physical_device.c/.h`, `vkr_device_memory.c`, `vkr_queue.c`,
  `vkr_ring.c`, `vkr_image.c`).

## Open questions

- Whether nixpkgs' Mesa builds Venus and Zink for aarch64-linux by default
  (it should: `vulkanDrivers` includes `virtio`).
- Whether the shell should sample the client's buffer directly or a
  helper-made IOSurface for full-screen clients (spike 4).
- GBM: Mesa's GBM on a virtio-gpu node without a display (render node
  only) is what nested Hyprland would use; untested.
