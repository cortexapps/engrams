-- ADR 0021 P1.5a: drop the harness_packs registry.
--
-- Pre-0021, harnesses lived in `harness_packs` (added by 0010) as a
-- deployment-wide `(name -> OCI registry URI)` index. The host-agent
-- pulled the pack on first use, built an ext4 substrate, and hot-
-- swapped it per session via FC `PATCH /drives` (option-D).
--
-- ADR 0021 retires that whole subsystem: the harness is baked into
-- each image at image-bake time and travels in the rootfs. There is
-- no per-deployment harness registry anymore — built-ins live in a
-- hardcoded catalog inside `engram-image-builder`, and custom
-- harnesses ride in via the author's Dockerfile.
--
-- Clean-cutover per the ADR's no-backwards-compat stance.

DROP TABLE IF EXISTS harness_packs;
