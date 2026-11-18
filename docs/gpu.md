# GPU: Vulkan through Venus, GL through Zink

Status: steps 1 to 4 done (2026-11-14, below; step 4's latency within
its target, to confirm on an idle machine): Linux Vulkan and GLES
programs under macwl-run draw on the Mac's GPU through the runtime's own
render node. ROADMAP item 11. Costs below are still estimates from measured ones (`docs/processes.md`,
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
KosmicKrisp (Mesa 26.2+, Vulkan 1.4 conformant, macOS 26+ and Metal 4;
geometry shaders only from Mesa main, step 1).
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

(Since 2026-11-18 the compositor does: macwl-run sets
`venus_implicit_fencing=true`, so step 3's wait is gone and the shell
waits instead. See "Implicit fencing" below.)

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

1. Zink over Venus over KosmicKrisp: GL 3.3 and GLES 3.1 with our
   transform feedback patch (below; 2.1 and 2.0 without), correctness
   beyond gears untested. Decides Firefox, GTK 4 and Gazebo.
2. virglrenderer's macOS support: young; its dmabuf assumptions (the vtest
   port was small: step 1).
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
   render server as broker. Done (below).
   Pass: Mesa logs no failed sync-file export; a deliberately slow frame
   never shows half drawn in headless dumps; `vkWaitForFences` wakes in
   under 30 µs p50 with no `nanosleep` under `MACWL_STRACE`; killing the
   worker mid-frame gets the guest `VK_ERROR_DEVICE_LOST` within a frame,
   not a hang.
3. **Overhead**: one benchmark (vkoverhead or a 10,000-draw scene, and one
   fill-bound scene) through the guest and natively on KosmicKrisp. Done
   (below): 91–95% draw-bound, 95–100% GPU-bound.
   Pass: 85% of native or better draw-bound, 95% GPU-bound; fence wake p50
   under 30 µs, p99 under 150 µs; no system calls per frame but present's.
   Fail: the case for the split driver.
4. **Zero-copy presenting**: the three extensions in virglrenderer,
   linux-dmabuf in the compositor. Done (below; one extension was
   enough), latency ~1.5–2 ms over native. Key press to colour change from a guest
   Vulkan app on the `presentedTime` harness, against `native-probe metal
   --no-capture`.
   Pass: p50 within 1 ms and p90 within 2 ms of native (6.2 / 10.5 ms).
   Compare rendering to linear, a blit to linear, and an IOSurface as the
   layer's contents.
   Fail: IOSurfaces over a Mach-port channel to the shell, and
   `MTLSharedEvent` ordering.
5. **Apps**: GTK 4's GL renderer, Firefox with WebRender on GL, Gazebo
   (GL, then `--render-engine-api-backend vulkan`), DXVK.

## Step 1, 2026-11-14

Venus over vtest, no runtime work but a fix. `gpu/vtest.sh SOCKET` builds
and runs the Mac side; the guest is stock nixpkgs Mesa 26.2.6 for
aarch64-linux (`VN_DEBUG=vtest`, `VTEST_SOCKET_NAME`, its `virtio` ICD).

- **The port** (`gpu/virglrenderer-macos.patch`, on virglrenderer
  `01637fc`): ~40 lines. vtest builds with Venus alone (upstream wants
  vrend); its shm comes from `os_create_anonymous_file` (a POSIX shm
  object, sized at creation: macOS sizes one only once); and an eventfd is
  a FIFO opened read-write and unlinked, one fd that's written, polled and
  passed to the client as an eventfd is (vtest reports no timelines
  without one, and Venus refuses a renderer without timelines). Venus and
  the render server built on macOS as they are.
- **KosmicKrisp**: nixpkgs' macOS Mesa builds it (26.2.6: Vulkan 1.4,
  conformance 1.4.3.2 on the M5 Pro), but without geometry shaders; Mesa
  main (`c9ce420`) has them, built by `gpu/kosmickrisp-main.nix`. Neither
  has `VK_EXT_transform_feedback` (main fills in its properties but
  doesn't advertise it).
- **The runtime**: a Unix socket path the guest gives can grow past
  macOS's 104 bytes on its way to the host's (Linux allows 108). Such a
  path now goes through a short alias of its directory, a symlink in
  `/tmp/macwl-UID/s/` (macOS has no public `bindat`/`connectat`), and
  reads back as the guest's (test `a_unix_socket_path_longer_than_macos_allows`).
- **Vulkan**: `vulkaninfo` in the guest sees "Virtio-GPU Venus (Apple M5
  Pro)", Vulkan 1.4; Venus passes on all KosmicKrisp's device extensions
  but the host-memory, Metal and fd ones it handles itself. `vkcube`
  draws right in the headless compositor (through the software WSI path:
  a copy into `wl_shm`, as expected without the dma-buf extensions).
- **dEQP-VK** (vulkan-cts 1.4.6.1, 3882 cases: smoke, info, memory
  allocation, compute, basic synchronization, simple and instanced draws,
  a slice of the monolithic pipeline tests): 1899 pass, 1 fails, 1982
  not supported. The failure is `info.device_memory_properties`:
  virglrenderer's memory-type spoof puts its host-visible type, which
  isn't DEVICE_LOCAL, in the DEVICE_LOCAL heap; it wants a heap of its
  own (a fix for virglrenderer). Not supported: D24S8 (no such format on
  Apple GPUs; 1044), `vertexInputDynamicState` (440), DRM format
  modifiers (184, the WSI gap), and extensions KosmicKrisp lacks.
- **GL**: Zink loads on Venus and says **GL 2.1 and GLES 2.0**: GL 3.0 and
  GLES 3.0 need transform feedback (`max_stream_output_buffers`,
  `zink_screen.c`), and KosmicKrisp has none. Its GLSL level is 460, and
  with `MESA_GL_VERSION_OVERRIDE=4.6 MESA_GLES_VERSION_OVERRIDE=3.2` it
  says GL 4.6 core and GLES 3.2; `es2gears_wayland` draws right through
  Zink on the compositor. Custom border colours need
  `MESA_KK_EXPERIMENTAL=custom_border`.
- **Not measured yet**: the frame rate. In the headless compositor every
  frame waited ~213 ms for its frame callback: the test compositor (a
  debug build writing each frame as a 7 MB PPM) set the pace, not the GPU
  path. Step 3 measures it.

So against step 1's criteria: Vulkan 1.4 (pass); the dEQP-VK subset 99.9%
of what runs (pass); Zink GL 3.3 core and GLES 3.0 (fail without an
override: transform feedback); a GLES client's pixels (pass); an image
bound to type 1 (not reached by these cases).

**Transform feedback is the gap that matters.** Firefox (WebRender: GL 3.1
or GLES 3.0), GTK 4 (GL 3.2 or GLES 3.0) and Ogre2 (GL 3.3) need the
version but don't use transform feedback, so the override gets them
going; a real fix is `VK_EXT_transform_feedback` in KosmicKrisp, which
already runs geometry shaders as compute (Honeykrisp, Asahi's Vulkan
driver whose geometry code KosmicKrisp shares, does transform feedback
the same way). Upstream work, and the next thing worth doing for GL.

## Step 2, 2026-11-14

The render node, in `linux-abi` (`crates/linux-abi/src/drm/`). `gpu/run.sh
PROGRAM` runs a Linux program with it (`gpu/build.sh` builds the Mac
side and prints the environment macwl-run needs: `MACWL_GPU`, the
directory with `libvirglrenderer.1.dylib` and `virgl_render_server`;
`VK_ICD_FILENAMES` for KosmicKrisp; `MACWL_GPU_DYLD_LIBRARY_PATH`, the
Vulkan loader, since dyld drops `DYLD_*` from an entitled process).
Fable (a second model) checked the uapi against Mesa and libdrm first;
the checklist held.

- **Finding it** (`drm/vfs.rs`): `/dev/dri/renderD128` (a character
  device, 226:128) and the sysfs libdrm's `drmGetDevices2` reads, as a
  virtio-gpu on PCI (1af4:1050): `/sys/dev/char/226:128/device/{drm,
  subsystem -> .../bus/pci, uevent (PCI_SLOT_NAME), vendor, device, ...}`,
  links followed inside the tree. While the GPU is on, `/sys` is this tree
  (a new resolver target, `Target::Dev`, beside `/proc`'s).
- **The node** (`drm/node.rs`, `drm/virgl.rs`): `VERSION` (virtio_gpu),
  `GET_CAP`, `GETPARAM` (blobs, host-visible, context init, alignment
  16 KiB), `GET_CAPS`, `CONTEXT_INIT`, `RESOURCE_CREATE_BLOB` (its inline
  command first), `RESOURCE_INFO`, `MAP`, `EXECBUFFER`, `GEM_CLOSE`, PRIME
  both ways. virglrenderer is `dlopen`ed once per host, in proxy mode: its
  render server is `posix_spawn`ed by `get_server_fd` (never a fork), and
  spawns a worker per context. Every call into it holds one lock; fences
  come back on its sync threads. A blob is the shm fd virglrenderer
  exports; `mmap` of the node at `MAP`'s offset maps that fd as a shared
  window.
- **Sync** (`drm/sync.rs`): fences, timeline syncobjs (create, destroy,
  wait, timeline wait with WAIT_FOR_SUBMIT, query, signal, transfer,
  reset, fds both ways), sync files (a socket pair's near end, readable
  once the fence signals; MERGE, FILE_INFO), and the dma-buf sync ioctls
  on blob fds. Waits block as the runtime's other waits do (parked, made
  again). Found by the CTS: a syncobj's absolute timeout is the guest's
  CLOCK_MONOTONIC, the host's UPTIME_RAW (the host's MONOTONIC runs ahead,
  and every wait timed out at once); and a reset drops a timeline's every
  point (Venus resubmits point 1).
- **Results**: `vulkaninfo` (Venus: "using DRM device
  /dev/dri/renderD128"); `vkcube` draws right in the headless compositor;
  the dEQP-VK subset of step 1 as there (1899 pass, the one failure
  virglrenderer's memory heaps), all 21 basic synchronization cases that
  run pass. Test `venus_through_the_render_node`.
- **Fence round trips** (`gpu/fencebench.c`: submit a command buffer with
  a fence and wait for it; the same source natively on KosmicKrisp):

  | p50 (p90) | KosmicKrisp natively | through the node |
  |---|---|---|
  | empty command buffer | 27–30 µs (36–50) | 68–97 µs (93–161) |
  | 64 MiB fill | 455–465 µs (585–663) | 496–522 µs (659–872) |

  So ~40–70 µs a synchronous round trip on top of native: more than this
  doc's estimate for the wake alone (15–25 µs), under a tenth of a
  millisecond a frame. The hops: the guest's ioctl, the proxy's socket to
  the worker, the worker's ring and queue threads, its retire through the
  FIFO "eventfd" to the proxy's sync thread, the callback, the guest
  task's wake. Not yet broken down: step 3.
- **A worker's death** (2026-11-14; Metal aborts a process on misuse,
  the reason contexts are processes of their own): the guest is told at
  once. virglrenderer's proxy retired a dead context's fences as
  *signalled* (after 100 stalled wakeups), and its sync thread then spun on
  the hung-up socket; and on macOS it never woke at all, `poll` with no
  events asked for not reporting a stream socket's peer going. Our
  virglrenderer patch (`gpu/virglrenderer-macos.patch`) watches the socket
  with a kqueue (EVFILT_READ with a low-water mark no reply reaches: only
  its end fires), stops watching once it's gone, and says the context is
  lost before retiring its fences: a fence id of `UINT64_MAX` on the fence
  callback. The runtime then fails the context's fences (the guest's waits
  get ENODEV: `VK_ERROR_DEVICE_LOST`) and its submissions. Test
  `a_gpu_workers_death_is_a_lost_device`: the worker killed under
  `fencebench`, which gets -4 within a second. A guest waiting on Venus's
  *ring* when it happens (shared memory, polled in the guest) isn't
  reached by any of this: Venus's own watchdog aborts it, after ~14 s, as
  it would on Linux.
- **Not checked yet**: EGL on Wayland and GBM through the node (Zink on
  the surfaceless platform works: GL 3.3, below).

## Step 3, 2026-11-14: overhead

Two benchmarks, each the same source built for Linux (under macwl-run,
through the render node) and natively for macOS (KosmicKrisp directly):
`gpu/fencebench.c` (a submission and a wait for its fence, over and over)
and `gpu/drawbench.c` (offscreen 1920x1080, three frames in flight,
command buffers recorded anew each frame: DRAWS tiny triangles with
their push constants, or PASSES full-screen ones of a heavy fragment
shader). `gpu/build.sh` builds them (glibc ones for Linux from nixpkgs'
cache, so no Linux builder; SPIR-V with glslang).

**Throughput** (fps, two runs each): the pass criteria hold.

| | natively | through the node | |
|---|---|---|---|
| 10,000 draws a frame | 180–189 | 171 | 91–95% (criterion 85%) |
| 1,000 draws a frame | 4380–4512 | 3538–3563 | 79–81% |
| 16 full-screen passes | 1653–1694 | 1565–1696 | 95–100% (criterion 95%) |

What the small frames pay is a fixed ~50 µs a frame: the round trip
below, 0.05 ms of a 120 Hz frame's 8.3.

**A fence's round trip** (fencebench, empty command buffer, p50): 25–27 µs
natively, 64–81 µs through the node. `MACWL_GPU_TRACE=1` (macwl-run's
environment; `=2` also logs execbuffers) stamps each fence through its
execbuffer, virglrenderer's callback and the guest's wait, and prints the
segments as the context goes (`drm/trace.rs`):

| p50 | |
|---|---|
| execbuffer, before virglrenderer | 1.7 µs (5.2 before reading the struct in one read: each read of guest memory walks its page tables) |
| `virgl_renderer_submit_cmd` | 2.8 µs (a one-way socket send) |
| making the fence (`context_create_fence`) | 4.4 µs (another) |
| fence made → signalled (worker, GPU, proxy) | 33–41 µs (the native round trip is ~27: ~6–14 µs in vkr's ring and sync threads, the FIFO "eventfd" and the proxy's sync thread) |
| signalled → the guest's wait returns | 9–10 µs (the runtime's wake, as for any wait) |
| the rest, in the guest | ~10 µs: Venus encodes `vkQueueSubmit` on its ring, then makes *two* execbuffers a submission (`vkSubmitVirtqueueSeqnoMESA` on ring 0, `vkWaitRingSeqnoMESA` on the queue's ring 1 with the fence), the syncobj reset |

Nothing left is one big cost. What would cut it: the eventfd as a kqueue
user event rather than a FIFO (the proxy and worker are ours to patch),
the two execbuffers' sends as one (they're consecutive), a short spin
before a fence wait sleeps (trading a CPU for ~5 µs; not by default).
Not done: none is worth it until presenting (step 4) says the frame's
budget needs it.

## Step 4, 2026-11-14: presenting

**Zero copy.** The guest's Wayland WSI is on its *prime* path: it renders
to an optimal image, then copies it on the GPU into a linear buffer
exported as a dma-buf, which it presents with `zwp_linux_dmabuf_v1`. Here
that buffer is a GPU blob, shared memory, which the shell maps and draws
from as it does a `wl_shm` pool (`Host::frame_dmabuf`); other hosts get a
copy. (Linear *images* on host memory, the other way, KosmicKrisp can't
do; buffers it can.) Fable traced what selects the path in Mesa's WSI and
what virglrenderer needed:

- virglrenderer (`gpu/virglrenderer-macos.patch`): advertises
  `VK_EXT_external_memory_dma_buf` (not image modifiers: with modifiers
  hidden, WSI takes the prime path whatever device the compositor names),
  takes it and `VK_KHR_external_memory_fd` out of the driver's device
  extensions again, allocates any memory the guest may export as host shm
  imported as host memory (whatever its type: WSI's sync-file probe uses
  the device-local one), and makes dma-buf buffers host-allocation ones.
- The compositor: `zwp_linux_dmabuf_v1` v4+ with default feedback naming
  the guests' render node (226:128; the vendored Smithay now sends a
  dev_t as Linux's 8 bytes), XRGB/ARGB8888 with LINEAR and INVALID (what
  prime buffers carry) modifiers, one plane. WSI's stride is 256-byte
  aligned, offset 0: what the shell's linear textures need.
- The runtime, found on the way: a sync file holds its fence (Venus resets
  the syncobj right after exporting one); a timeline's point 0 is its last
  fence (Venus exports point 0; without this the export failed and WSI
  committed frames the GPU hadn't finished: black frames in the dumps); an
  empty execbuffer is a fence alone; `fstat` of the node is the node
  (libdrm finds the device by its number; Zink on EGL Wayland failed).
- Zink presents the same way (kopper on Venus's WSI): `es2gears_wayland`
  right in the dumps.
- Test `a_gpu_client_presents_finished_frames_as_dma_bufs`:
  `gpu/keyframe.c` (a Vulkan client that draws a new colour for each key)
  under the headless compositor, typed at; each frame's colour right, each
  buffer a blob.

**Latency** (`gpu/latency.sh`: keyframe in macwl-shell, the shell typing
40 keys and timing each to its answer's `presentedTime`; 120 Hz). Today
the machine ran slower than the Done table's day (native 10–11 ms p50
against 6.2 then), so these compare with each other:

| p50 / p90, ms | |
|---|---|
| native Metal (`native-probe metal --no-capture 30`, 4 runs) | 10.0–11.2 / 15.7–16.7 |
| the shell's own (`macwl-shell --latency --auto 30`) | 11.3 / 17.0 |
| keyframe through the node, FIFO (3 runs) | 12.5–12.8 / 18.1–19.0 |
| keyframe, mailbox (2 runs) | 13.6–14.6 / 18.4–19.4 |
| keyframe, the shell copying (`MACWL_SHM_COPY=1`) | 16.0 / 21.2 |

So a guest GPU client is ~1.5–2 ms behind native at p50 and ~2 ms at p90:
just outside step 4's target (1 and 2 ms). The gap is the client's own
key → commit, 1–2.5 ms, where a native app presents from inside its
command buffer: the key's way to the guest, Venus's present thread, the
blit, the fence wait, the commit. Zero copy is worth ~3.3 ms over copying.
Mailbox is no faster. What would close it: see where key → commit goes
(`MACWL_GPU_TRACE` covers the GPU part); the blit could go if the shell
sampled the optimal image itself (an IOSurface, a Mach-port channel); and
the composite pass could go for a full-screen client (its buffer as the
layer's contents).

### Under 1 ms? (2026-11-14, continued)

Traced on one clock (the host's uptime: `MACWL_KEY_TRACE=1` in the shell
and compositor, `KEYFRAME_TRACE=1` in keyframe, `NATIVE_TRACE=1` in
native-probe, the system call log), on the direct full-screen path:

- **The GPU wakes slowly.** After ~150 ms idle, even native KosmicKrisp
  takes ~1.4 ms for a frame's clear and copy (0.2 warm); the GPU's own
  timestamps say the work is short, so it's the wake. The shell now
  starts a tiny GPU job the moment it sends a key (on by default,
  `MACWL_GPU_PREWARM=0` off): key → commit 2.4 → 1.7 ms. Keeping it warm
  while typing (`MACWL_GPU_KEEPWARM_MS`) gains ~0.15 ms more, not by
  default; bigger warm-up jobs don't help.
- **The compositor held keys** for the rest of its loop's turn (~180 µs)
  before flushing them: it flushes at once now.
- **The shell's frames went through compositing**: its compositor mode
  blitted the window into the drawable, so the drawable couldn't be
  framebuffer-only. GPU done → glass was ~9.3 ms against native's 6.3; the
  window is now drawn by the same quad pass as popups, the drawable is
  framebuffer-only, and it's 6.4. (This is every client's latency, not
  only GPU ones'.)
- What's left, on the direct path (under CPU load from other work): native
  key → glass p50 7.2 ms, keyframe 9.5 (key → commit 2.0 loaded, 1.7 idle;
  the shell's composite 0.5 ms; then as native). Native's own render is
  ~1 ms of its total. So ~1.2–1.5 ms over native, not yet under 1.
- **Measured again** (later the same day, another workload keeping the
  GPU ~37% busy, so neither side woke a cold GPU; samples on the direct
  path only): native Metal key → glass p50 6.96 / p90 10.73 ms (43
  samples), keyframe through the node 7.61 / 9.96 ms (99 of 200); its key →
  commit 1.09 ms. **0.65 ms over native at p50**, under step 4's 1 ms; to
  confirm on an idle machine, where the GPU's wake is back (the warm-up
  covers most of it). The shell now draws its background and surfaces in
  one pass (no separate clear pass; the composite's 0.5 ms is reading the
  client's frame, not the clear).
- **On an idle machine** (2026-11-18; plugged in, nothing heavy running):
  three alternating pairs of 200 keys, native `native-probe metal
  --no-capture 200` against `gpu/latency.sh 200`. Native p50 6.97, 5.92,
  6.53 ms; keyframe 7.23, 7.61, 7.82; its key → commit 0.98–1.08. Means:
  p50 6.47 against 7.55, **~1.1 ms over native**; p90 9.84 against 10.26
  (+0.4); min 2.69 against 3.57 (+0.9, the steadiest: a key's phase in
  the 8.3 ms frame moves p50 by ±0.5 ms between runs of the same thing).
  So the warm 0.65 doesn't hold once the GPU sleeps between keys: ~0.9–1.1
  ms over native, at the target rather than under it. (A fourth native run,
  p50 20.75 and min 16.6, went through compositing and is left out.) Key
  → commit is most of the difference; the warm-up covers part of the
  GPU's wake, not all.
- Where a blocked guest thread's wake goes after ~150 ms idle
  (`MACWL_WAKE_TRACE=1` times guest runs): 81 µs from a message to the
  guest's next clock reading, native 35; ~10 of it is waiting in `poll`
  rather than `read` (native measures the same difference), ~11 entering
  the guest after the host thread slept, ~20 the dispatcher. Small next to
  the GPU's wake, which a key's answer waits on anyway.
- An IOSurface shown as the layer's contents (no composite) was looked at
  with Fable: feasible with public APIs (an IOSurface's Mach port handed
  over a bootstrap name from the worker to the shell; KosmicKrisp writing
  it through a no-copy buffer over its memory), but it saves at most the
  0.5 ms composite, may cost a frame in Core Animation's transaction, and
  can't be timed with `presentedTime`. Not built.
  Tried natively on 2026-11-18 (see "IOSurface direct display" below):
  it does cost the frame.
- Left to try: the runtime's wake of a blocked guest thread (102–132 µs
  against native's 45 after idle), Venus's per-frame CPU work (~0.35 ms),
  the prime path's blit (rendering straight to linear memory).

### Implicit fencing, 2026-11-18

Where the idle machine's ~1 ms over native went, traced (`MACWL_KEY_TRACE`,
`KEYFRAME_TRACE`, `MACWL_STRACE` on the uptime clock; the shell now also
prints Metal's own scheduled / GPU start / GPU end times):

- The key reaches keyframe ~215 µs after the shell sends it (most of the
  compositor's apparent ~110 µs was the trace's own `eprintln!`s), and
  keyframe's acquire, record, submit and present take ~200 µs.
- Then ~560 µs to the commit: Venus's present thread blocks on the frame's
  sync file (`vn_wsi_sync_wait`, because virtio-gpu has no implicit
  fencing) until the GPU has drawn it and the fence has come back, then
  imports it into the dma-buf and commits.
- The shell's composite: ~0.1 ms encoding, ~0.1 ms to the GPU scheduling
  it, and a steady 0.247 ms on the GPU: a full-screen read of the client's
  linear buffer and write of the drawable (3024×1964, 48 MB, ~190 GB/s:
  bandwidth). Native only clears.

Venus can skip its wait: drirc's `venus_implicit_fencing` (default off)
makes it present without the async present thread, the fence going into
the dma-buf (`IMPORT_SYNC_FILE`) and the commit out at once; the
compositor is then to wait, as a Linux compositor waits on a dma-buf's
fences. Explicit sync (`wp_linux_drm_syncobj_v1`) isn't an alternative:
Venus exports no timeline semaphores, so Mesa's WSI never uses it.

So macwl-run sets `venus_implicit_fencing=true` for guests when the GPU is
on, and tells the compositor of each imported fence over a socket next to
the Wayland one, `$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY.fences`
(`linux-abi` `drm::fences`, `macwl_compositor::fences`): `WAIT seq key`
as the client imports it, before its commit, and `DONE seq` once it has
signalled. A buffer's key is what names its memory in both processes: a
POSIX shm object's name, which macOS keeps after unlinking and reports
through `proc_pidfdinfo(PROC_PIDFDPSHMINFO)` (`fstat` says device 0,
inode 0 for every one), or a file's device and inode. The WAIT is in the
fences socket before the commit is sent, so the compositor reads that
socket before looking (no race between the two). The shell encodes the
composite at once with `encodeWaitForEvent` on an `MTLSharedEvent` it
signals as DONE comes: the GPU starts reading the moment the client's
drawing is done, the encoding and scheduling already behind it. A
connection that ends signals all it was waiting on (no shell hung on a
dead runtime); a lost device fails its fences, which send DONE too. The
compositor's CPU path (a buffer it copies) waits on the CPU. With no
compositor listening, the import waits for the fence itself, as Venus
would have, so turning it on is safe with any compositor.
`DMA_BUF_IOCTL_EXPORT_SYNC_FILE` now gives a buffer's last imported fence.
`MACWL_ENV_venus_implicit_fencing=false` turns it off.

Measured as key → GPU done, which a key's phase in the frame doesn't move
(glass is the next refresh after it: its p50 moves ±0.5 ms between runs
of the same thing), interleaved, two rounds of 150 keys each, idle
machine:

| key → GPU done, p50 | round 1 | round 2 | all | over native |
|---|---|---|---|---|
| native Metal (`NATIVE_TRACE`) | 0.86 | 0.73 | 0.82 ms | |
| keyframe, Venus waiting | 1.88 | 1.94 | 1.90 ms | +1.08 |
| keyframe, implicit fencing | 1.34 | 1.40 | 1.35 ms | **+0.53** |

Key → commit 1.17 / 1.13 → 0.46 / 0.53 ms; key → glass p50 in the same
runs 8.30 / 9.08 → 7.96 / 7.89. **About 0.5 ms over native**, under step
4's 1 ms, with the GPU not kept warm. (Both guest modes had long tails in
these runs, p90 ~5 ms, as the machine's other work had then.) What's left
is mostly the composite's full-screen copy (~0.25 ms GPU) and the
client's ~0.2 ms of Venus CPU work.

### IOSurface direct display, 2026-11-18

The composite left after implicit fencing (~0.25 ms on the GPU, a full-
screen copy at memory bandwidth, plus ~0.2 ms encoding and scheduling)
would go if the client's buffer were an IOSurface the shell's layer showed
as its `contents`. Whether that's as quick to glass as a framebuffer-only
drawable was the question, tried natively before any plumbing
(`native-probe`):

- A layer's `contents` has no `presentedTime`, and ScreenCaptureKit makes
  WindowServer composite, so neither times it. Watching the replaced
  surface go out of use (`IOSurfaceIsInUse`) looked like a signal (for a
  drawable it lagged `presentedTime` by ~0.5 ms once) but wasn't steady:
  a drawable's surface was sometimes released 0.08 ms after the event, and
  the contents path's median moved 5.3 → 8.9 ms between identical runs.
  Nor do a CVDisplayLink's timestamps mark the refreshes (`presentedTime`s
  fall evenly across its period).
- What does tell: a full-screen CAMetalLayer presenting inside a Core
  Animation transaction (`presentsWithTransaction`, the path a `contents`
  change takes, `NATIVE_WITH_TRANSACTION=1`), against presenting as usual,
  two pairs of 100: p50 17.26 / 15.75 ms against 7.42 / 7.30, min 5.28 /
  4.15 against 3.53 / 3.15. **About a frame slower.**

So showing a client's IOSurface through Core Animation would trade the
~0.4 ms composite for a frame (8.3 ms). Not built. Saving the composite
would need the client's frame drawn into the drawable itself (the
drawable's IOSurface handed to the GPU worker each frame), which
CAMetalLayer's ownership of its drawables makes awkward.

## Transform feedback in KosmicKrisp, 2026-11-14

`gpu/kosmickrisp-transform-feedback/` (ours, ten commits on Mesa
`c9ce420` meant for upstream, applied by `gpu/kosmickrisp-main.nix`;
written by a subagent in its own Mesa checkout, then revised for review:
the passthrough GS built on first capture, a NIR io option in place of an
output-trimming workaround, no compute path while capture is off): `VK_EXT_transform_feedback`, captured the way Honeykrisp
does on top of the shared geometry shader emulation (`src/poly`): the GS
rasterization vertex function writes the captured outputs, after the GS
count pass, its prefix sum and the pre-GS program have placed each
primitive and advanced the buffer offsets and stream queries. A pipeline
capturing from its VS or TES gets a passthrough GS. Counters, stream
queries, `vkCmdDrawIndirectByteCountEXT` and `geometryStreams` work;
`transformFeedbackStreamsLinesTriangles` doesn't, nor do primitive counts
for draws that don't capture.

- dEQP-VK `transform_feedback.simple.*` and `primitive_restart`: 4983
  pass, 0 fail (2912 not supported: mostly more than 4 streams);
  `fuzz.*`: 830 pass, 0 fail; a smoke set (geometry, draws, clipping,
  tessellation, queries): as without the patch.
- **Zink now says GL 3.3 core and GLES 3.1 without an override**,
  through the render node (was 2.1 and 2.0): enough for GTK 4, WebRender
  and Ogre2. What holds it below GL 4.0 and GLES 3.2 now is
  `ARB_texture_buffer_object_rgb32`: RGB32 texel buffers, which
  KosmicKrisp enables only on Apple GPU family 11 (`kk_format.c`); the M5
  Pro is family 10.
- Upstreamable with work: a reviewer would likely want the passthrough GS
  made at draw time (as Honeykrisp does) rather than at pipeline creation,
  the output trimming (a Metal linking gap) fixed in `nir_opt_varyings`,
  and primitive counting for draws that don't capture. Any capturing
  pipeline takes the compute VS + GS path, even with capture off.

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
