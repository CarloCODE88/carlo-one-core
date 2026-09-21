# HIXX-Native

This directory contains the archived HIXX HTTP-to-kernel-IPC prototype
integrated as a separate backend/research component of CarloONE.

HIXX is not the productive inference backend. It currently provides:

- a loopback HTTP daemon,
- a Rust ioctl/mmap client,
- a Linux character-device kernel module,
- a 4 MiB shared-memory mapping, and
- init, submit, and status operations.

The module/device namespace and ABI must remain separate from the triAI
engine until an explicit versioned integration decision is approved.

Build and runtime requirements are documented in the public docs repository:

- `carlo-one-docs/operations/HIXX-SERVER-ARCHITECTURE-AND-OPERATIONS.md`

Generated Rust and kernel artifacts are intentionally excluded from Git.
