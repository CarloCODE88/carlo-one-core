# CarloONE backend source inventory

This repository carries the reusable source foundations for the CarloONE
backend under two deliberately separate directories:

## `triAI-Engine/`

The complete reusable userspace model-runner and server source is imported
from `/home/carlos/PROJEKTE/triAI-Engine`. This includes its Rust modules,
HTTP/API layer, worker supervisor, model catalog and registry, prompt
guardrails, resource planning, staging/chunking logic, tests, benches,
configuration examples, deployment template, scripts, and technical
documentation.

Large model files (`*.gguf`, `*.bin`) and generated build outputs are not
source and are intentionally excluded. They must be supplied separately via
the model/artifact distribution process.

## `hixx-native/`

The complete reusable HIXX HTTP-to-kernel-IPC source is imported from
`/home/carlos/PROJEKTE/hixx-native`. This includes the Rust daemon, API,
ioctl/mmap client, shared-structure definitions, kernel C sources, Makefiles,
lockfile, and tuning script.

Kernel objects, module metadata, Rust `target/` output, logs, PIDs, and other
generated files are intentionally excluded.

## Runtime boundary

These are source foundations, not a claim that the combined system is already
production-ready. `triAI-Engine` remains the productive model-supervisor
direction. HIXX remains a separate IPC/prototype component until its module
namespace and ABI relationship to triAI are explicitly decided.
