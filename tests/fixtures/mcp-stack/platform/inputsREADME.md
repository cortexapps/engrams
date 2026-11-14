# Staged platform Inputs

`.github/scripts/mcp-stack-inputs.mts` writes the private Inputs JSON that the
platform adapter consumes. It runs offline and uses the actual `parseInputs`,
`BINARIES`, and `IMAGES` from [preflight.ts](./preflight.ts). A successful write
establishes file hashes and schema validity. It does not qualify an engine, an
image, a host, or a physical backend.

## Template and final manifest

The trusted staging job supplies a private JSON template with the current
`Inputs` fields. The only difference is the shape of each file pin:

| Location                               | Template                                       | Final Inputs                                                           |
| -------------------------------------- | ---------------------------------------------- | ---------------------------------------------------------------------- |
| Each `binaries` entry                  | `{ "path": "/canonical/staged/file" }`         | `{ "path": "/canonical/staged/file", "sha256": "<file SHA-256 hex>" }` |
| `proxy`, `firecracker`, `nativeProbe`  | The same path-only pin                         | The same final file pin                                                |
| Each `images` entry's `archive`        | The same path-only pin                         | The same final file pin                                                |
| Each image's `id`                      | Explicit config digest from the producer       | Preserved                                                              |
| Each image's `manifestDigest`          | Explicit raw manifest digest from the producer | Preserved                                                              |
| `sourceCommit` and other Inputs fields | Explicit values from the CI stage              | Preserved and checked by `parseInputs`                                 |

Supply every binary role in `BINARIES` and every image role in `IMAGES`.
Firecracker also requires the `firecracker` pin and the `images.hostAgent` entry;
it must omit `nativeProbe`. VZ requires `nativeProbe` and `architecture: "arm64"`;
it must omit `firecracker` and `images.hostAgent`. Both backends require `proxy`.
Missing fields, extra fields, and caller-supplied `sha256` fields fail validation.

Each image has three separate facts. The writer hashes the OCI archive file to
produce `archive.sha256`. The actual OCI producer supplies `manifestDigest` as
`sha256:` plus the hash of the raw single-platform manifest bytes. It supplies
`id` as `sha256:` plus the config blob hash, which must match the loaded Docker
image ID. The writer does not derive, replace, or validate the linkage of these
two producer facts. Do not substitute the archive hash for either digest.

## Invocation

Use Node 24. The transform option is necessary because the shared platform
modules use TypeScript parameter properties.

```sh
node --experimental-transform-types .github/scripts/mcp-stack-inputs.mts \
  --backend firecracker \
  --template /private/staging/template.json \
  --output /private/staging/inputs.json
```

Use `--backend vz` for VZ. The flags have this exact order; no additional flags
are accepted. Both JSON paths must be absolute and canonical. The template must
be a nonempty, owned, single-link regular file of at most 1 MiB, with no group or
other permission bits. Its path and parent components must not be symlinks.

Every staged artifact must be a nonempty, owned, single-link regular file at a
canonical path with no symlink components. The writer streams each file through
one descriptor and checks its identity, size, and modification/change times.
Make owned copies of downloads and tool binaries; do not use symlinks or hard
links. The writer does not copy files, discover tool paths, load images, execute
commands, or read a home directory or cloud credentials.

The output parent must already be an owned canonical directory with mode 0700.
The output path must be absent. After all file and schema checks pass, the writer
creates the manifest exclusively with mode 0600. It never replaces an existing
file or link. Success has exit code zero and no standard output. Failure has a
nonzero exit code and does not report platform acceptance.

## CI staging and remaining gates

Run the writer as the same UID that runs the platform adapter. Firecracker
preflight requires root: stage actual root-owned file copies, the root-owned
private template, and the root-owned 0700 output parent before running the writer
as root. Running a normal-runner-owned template through `sudo` is insufficient.
For VZ, stage and write as the native arm64 macOS runner UID. Sign the final VZ
host binary before hashing it. Retain executable modes on executable artifacts.

Real producer jobs must still build or obtain the complete binary and image set,
export single-platform OCI archives, collect their raw manifest/config digests,
and load the same images into the selected engine. The stage must supply the
actual source commit, engine identity/socket/config, private network addresses,
and routing prerequisites. The writer preserves `sourceCommit`; it does not
discover HEAD or certify a clean checkout.

The existing preflight and asset staging checks must still verify clean matching
HEAD, source revision labels, binary architecture and versions, signatures and
entitlements, image/archive/manifest/config linkage, loaded engine identity, and
the selected backend prerequisites. The actual producer/consumer workflow edges,
native DNS and routes, and required Firecracker and VZ adapter runs remain
necessary. No supplied PASS field replaces those checks. This source tool does
not establish image production or physical acceptance.
