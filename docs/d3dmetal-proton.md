# Windows games: Proton with D3DMetal for graphics

Status: design (2026-11-01). Steps 0 and 1 done: D3DMetal runs in a plain
Mac process through d3dmetal-native, and its frames reach macwl-compositor
as ordinary `wl_shm` buffers (below). Depends on FEX running under
macwl-run (not started) for x86 games, but not for the graphics work.

## The goal

Open Steam in the macwayland session, pick **Proton-D3DMetal** in a game's
compatibility dropdown, and play it. Proton-D3DMetal is Valve's Proton with
one part swapped: DXVK and VKD3D-Proton (Direct3D on Vulkan) are replaced
by DLLs that tunnel the game's Direct3D calls out of the guest to Apple's
**D3DMetal** (the graphics part of the Game Porting Toolkit), which turns
them into Metal. Wine, FEX, Steam and the rest of Proton stay as they are.

Why not just use GPTK natively (CrossOver and the like)? For most games you
should. This is for what that path can't do or doesn't have: Proton's
per-game fixes, games whose anti-cheat supports Proton, and one environment
with the rest of the Linux apps. And why not DXVK on a Vulkan driver (ROADMAP
item 11)? That would also work, but it's two translations (D3D → Vulkan →
Metal) where D3DMetal is one, and Apple tunes D3DMetal for its GPUs.

## Shape

```
 Windows game (x86, emulated by FEX inside arm64 Wine)
   │  D3D11 / D3D12 / DXGI calls
   ▼
 proxy d3d11.dll, d3d12.dll, dxgi.dll (ARM64EC: native, not emulated)
   │  commands in a ring in shared memory
   ▼
 macwl-d3d: a Mac helper process, x86_64 under Rosetta
   │  the same calls, made for real
   ▼
 D3DMetal → Metal → shared texture (linear, in a shm fd)
   │  the fd, as a wl_shm buffer on the game's Wayland surface
   ▼
 macwl-compositor → macwl-shell
```

## D3DMetal, as found

- An x86_64-only Mach-O framework, so it needs Rosetta and can't be loaded
  into macwl-run (arm64, and Hypervisor.framework needs a native process).
  Hence the helper process.
- It exports the D3D entry points (`D3D11CreateDevice`, `D3D12CreateDevice`,
  `CreateDXGIFactory2`, `D3DCompile`, ...) with the **Windows x64 calling
  convention**, made to be called directly by Windows code in the same
  process (as GPTK's Wine does). Its structs are Windows' layouts. Windows
  on ARM64 lays its structs out the same way as x64, so the helper can
  mostly pass deserialized structs straight through, fixing up pointers.
- It reports `OpenExistingHeapFromAddress` (D3D12 heaps over memory you
  provide) and shared handles as unsupported.
- It expects a host: `GFXT_Initialize` takes an OS interface (instances,
  events, monitors, adapters, windows, registry, allocation) that GPTK's
  Wine provides. Without one it can't make a device.

### d3dmetal-native

[d3dmetal-native](https://github.com/utmapp/d3dmetal-native) (UTM's, MIT)
is that host for ordinary Mac processes: link it and the D3D entry points
work. On top of D3DMetal it adds what this design needs:

- pseudo-HWNDs for an `NSView` or a **caller-owned `CAMetalLayer`**, so
  swap chains can present into a layer we create offscreen;
- Win32-style events, waitable and exportable as fds (`dmn_event_dup_fd`);
- **cross-process sharing** of textures, buffers, fences and keyed mutexes
  through the standard D3D shared-handle APIs, which D3DMetal stubs out:
  a handle is a small struct whose one live resource is an fd, passed over
  a Unix socket;
- **`dmn_open_existing_heap_from_fd`**: a D3D12 heap over memory from an
  fd, the zero-copy path for upload heaps (see **Memory**);
- registry seeding and an executable-path override, so D3DMetal's per-app
  profiles match the game rather than the helper.

**Step 0, 2026-11-01** (M5 Pro, macOS 27, Rosetta; D3DMetal 3.0 as bundled
with UTM 5.0.8): built from source (`meson setup build --cross-file
build-macos-x86_64.txt`), its headless suite passes 51 of 55, and the
on-screen D3D11 triangle reads back right (`DMN_VALIDATE=1`). The failures:

- three fence tests whose harness forks without exec'ing: the child
  crashes ("crashed on child side of fork pre-exec"; Metal's shader
  compiler service isn't reachable across a bare fork). So **the helper is
  always a freshly exec'd process**, never a fork of one using Metal.
- `d3d12-atomic64`: 64-bit atomics (typed, groupshared and raw) give 0.
  The pipeline compiles; the results are wrong. Unreal Engine 5's Nanite
  needs these, so UE5 DX12 games may break with this D3DMetal.
- Licence: Apple's, and not redistributable. Proton-D3DMetal never ships
  it: d3dmetal-native loads it from wherever the user installed it
  (`D3DMETAL_FRAMEWORK_PATH`, or next to the library). The D3D headers
  come from mingw-w64/Wine, not GPTK. Nothing of D3DMetal goes in this repo.

## Pieces

### The helper (`macwl-d3d`)

A Mac program built for `x86_64-apple-darwin`, one per guest process that
creates a D3D device, linked with d3dmetal-native (which finds and loads
D3DMetal). It maps the shared ring and replays commands. Objects are named by IDs the guest allocates (so creating one
needn't wait for an answer); the helper keeps an ID → COM pointer table.

How a guest gets one: the guest can't start Mac programs except by
replacing itself (`exec_host`), so the runtime does it. Opening a device
file, `/dev/macwl-d3d`, makes macwl-run spawn the helper (host side, so it
works whether or not Mac programs are enabled; spawned, never forked: see
step 0) and returns a socket to it.
The ring is a memfd passed over that socket: guest `MAP_SHARED` memory is a
host `mmap` (`crates/runtime/src/shared.rs`), so the helper maps the same
pages.

### The proxy DLLs

`d3d11.dll`, `d3d12.dll`, `d3d12core.dll`, `dxgi.dll` (and later `d3d9`,
`d3d10core`), exporting what DXVK and VKD3D-Proton export. Each COM object
is a proxy holding an ID; its methods write a command (method number, ID,
arguments, inline data) into the ring.

Built as **ARM64EC**: Valve's arm64 Proton runs arm64 Wine with FEX
emulating only the game's x86 code, and ARM64EC DLLs are native code that
emulated x64 code can call directly. So serializing the calls costs native
time. The same DLLs built for ARM64 run under plain arm64 Wine with
Windows-on-ARM test programs, so all of this can be developed and tested
before FEX works.

Most calls go one way: draws, state, copies, creates (with the ID chosen
up front). Calls whose answer the game needs wait for the helper: `Map` for
reading, queries (`GetData`), `CheckFeatureSupport`, `GetDesc`-type calls
not cached guest-side, `Present`'s throttling. Games make these far less
often than draws.

### The ring

Single producer per device context (D3D11's immediate context and each
deferred one; each D3D12 command list records into its own buffer, sent on
`ExecuteCommandLists`). Large inline data (initial data, `UpdateSubresource`
sources) goes in a separate arena in the same shared memory.

Waking the helper: it spins briefly after the ring empties, then sleeps on
a host semaphore; the guest only signals (one system call, so a VM exit)
when a flag in shared memory says the helper is asleep. Waking the guest
for an answer: the helper can't wake a guest futex (guest futexes live in
the runtime), so the guest spins briefly on the answer slot, then blocks
reading the socket, which the helper writes to.

### Memory

The hard part, as for any API remoting.

- **D3D11 `Map` with `WRITE_DISCARD`/`NO_OVERWRITE`** (dynamic buffers,
  every frame): the guest writes into the arena; `Unmap` sends a copy that
  the helper does into its own mapping.
- **`Map` for reading**: waits; the helper copies into the arena.
- **D3D12 upload heaps** are mapped once and written for ever, with no
  `Unmap` to say when. The helper creates them over our shared memory:
  the guest's `ID3D12Device::CreateHeap` (and committed upload/readback
  resources, as placed resources on such a heap) becomes a memfd in the
  guest, sent to the helper, which makes the heap with
  `dmn_open_existing_heap_from_fd`. The game's mapped pointer is then the
  guest's own view of the pages D3DMetal reads: no copies. Fallbacks if
  that falls short (alignment, sizes, D3D11's dynamic resources):
  1. Share the helper's pages the other way: a Mach memory entry
     (`mach_make_memory_entry_64`) for D3DMetal's mapped pointer, mapped
     by macwl-run into guest physical memory as it does shared mappings.
  2. Track writes: a shadow copy write-protected in the guest's page
     tables (macwl-run owns them), copying only touched pages at
     `ExecuteCommandLists`. A fault per page per frame (~5 µs each) is
     too slow for big heaps without hardware dirty bits (FEAT_HAFDBS).

### Presenting

Simpler than it looked: on macOS, d3dmetal-native backs a shared texture
(`D3D11_RESOURCE_MISC_SHARED`, or a D3D12 shared heap) with a **linear
image in a shared-memory fd**, and its handle says where (`dxn_shared_texture`:
fd, offset, row pitch). That is exactly a `wl_shm` buffer. So the helper
renders into shared textures, the fds go to the guest, and Wine's Wayland
driver attaches them as ordinary `wl_shm` buffers: no private protocol, no
IOSurfaces, and the compositor needs nothing new.

The proxy forwards swap chain calls; in the helper the swap chain's back
buffers are shared textures (or are copied into some at `Present`, one GPU
blit, as d3dmetal-native's own present path does). `Present` waits for the
GPU (a fence; the step 1 probe uses an event query), then tells the guest
which buffer to attach; `wl_buffer.release` returns it.

Today the compositor copies each committed `wl_shm` buffer's damaged area
into a texture. Wrapping a shm pool in an `MTLBuffer`
(`newBufferWithBytesNoCopy`) and drawing from a texture over it would make
that zero-copy for every shm client, games included; that's the
compositor's IOSurface item done another way (ROADMAP, Compositor).

**Step 1, 2026-11-01** (`experiments/d3d-wayland`, not in the repo): an
x86_64 D3D11 program renders a spinning triangle into two shared textures
through d3dmetal-native and sends their fds over a Unix socket; an arm64
Wayland client (from `hello-client`) wraps them as `wl_shm` buffers, asks
for a frame on each frame callback, attaches it, and returns releases.
The headless compositor's dumps show the triangle, rotated as expected by
the last frame; 240 frames in 2.5 s, paced by the compositor's frame
callbacks (95 fps headless), at 640×480 (pitch 2560, offset 0).

### Packaging

A fork of Proton with the DLLs swapped, as a Steam compatibility tool: a
directory in `compatibilitytools.d/` with `compatibilitytool.vdf`,
`toolmanifest.vdf` and the `proton` script, which copies our DLLs into the
prefix instead of DXVK's and VKD3D-Proton's. The manifest doesn't ask for
the Steam Linux Runtime container (pressure-vessel needs namespaces, which
macwl-run doesn't have); libraries come from the guest root.

### A frontend

- **Mac Steam doesn't work:** it has no Steam Play (no compatibility
  tools), a passed-through Mac program leaves the session, and a game's
  `steam_api.dll` needs Proton's `lsteamclient`, which talks to the Linux
  client.
- **First:** no Steam UI. steamcmd downloads Windows builds
  (`+@sSteamCmdForcePlatformType windows`), umu-launcher runs them with
  Proton-D3DMetal. DRM-free games only.
- **Eventually:** Linux Steam under FEX in the session, with the dropdown.
  steamwebhelper is Chromium, so it's heavy, but Valve is making the Linux
  client run on arm64 with FEX already.

## Steps

0. **Probe** (done, 2026-11-01): D3DMetal works without Wine through
   d3dmetal-native; see above.
1. **On screen** (done, 2026-11-01): D3D11 frames from an x86_64 producer
   in an arm64 Wayland client's window, as `wl_shm` buffers over shared
   textures; see **Presenting**.
2. **The tunnel, without Wine.** `/dev/macwl-d3d`, the helper, the ring.
   A D3D12 heap over a guest memfd (`dmn_open_existing_heap_from_fd`).
3. **Proxy DLLs, D3D11.** Under arm64 Wine with Windows-on-ARM test
   programs: a triangle, then a D3D11 sample suite. Present through
   Wine's Wayland driver.
4. **FEX** (its own track): x86-64 Linux programs, then arm64 Wine with
   FEX for x86 Windows code.
5. **Proton-D3DMetal.** The Proton fork; a DRM-free DX11 game through umu.
6. **D3D12.** Upload heaps per Memory; command lists.
7. **Steam** under FEX, with Proton-D3DMetal in the dropdown.

Steps 1–3 and 4 are independent.

## Tests

- The helper replays recorded command streams (a test program's capture)
  and compares read-back pixels with a reference, so most of it is tested
  without Wine.
- Headless compositor screenshots, as for the other clients, for
  presenting.
- Test programs (C, built for Windows ARM64 with llvm-mingw) under arm64
  Wine: device creation, every map mode, queries, a deferred context,
  resizing the window.
- No D3DMetal in CI or the repo: tests that need it skip when it isn't
  installed.

## Open questions

- **TSO.** FEX needs x86's memory ordering. Apple's cores have a hardware
  mode for it (Rosetta uses it); can a Hypervisor.framework guest turn it
  on? Without it FEX orders memory in software, which is much slower for
  multithreaded games. Decides whether this is fast or a curiosity; worth
  testing early, though it's FEX's problem more than this design's.
- Pacing for real games: the waitable object and `Present`'s sync
  interval against the compositor's frame callbacks.
- 64-bit atomics (step 0): a D3DMetal bug or an M5 one, and whether a
  newer GPTK fixes it.
- Whether guest memfds meet `dmn_open_existing_heap_from_fd`'s needs
  (alignment, the heap sizes games ask for).
- One helper per device or per process; what happens when the helper dies
  (device removed, as on Windows).
- Proxy DLLs in C++ (COM vtables come naturally) or Rust.
- Rosetta's future: Apple has said it will be cut back after macOS 27,
  possibly keeping what games need. D3DMetal itself is x86_64 only today.
