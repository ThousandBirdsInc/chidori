# Chidori QuickJS Fork

This directory contains the in-repository QuickJS fork required by the
TypeScript VM snapshot runtime. The initial upstream source snapshot was copied
from `rquickjs-sys 0.9.0`'s vendored QuickJS tree so Chidori can own future VM
snapshot patches in this repository.

The current Chidori-specific C file is still a compile-time scaffold only. It
exposes the planned Chidori snapshot FFI symbols and returns explicit
unsupported results. The next fork work is to patch the vendored QuickJS sources
for heap, async-frame, promise, job-queue, and host-promise serialization, then
move the build from the scaffold-only library to the patched fork.
