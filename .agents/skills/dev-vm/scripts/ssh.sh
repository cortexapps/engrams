#!/usr/bin/env bash
# Convenience SSH. Drops into a shell at $REMOTE_DIR by default.
# Pass extra args after `--` to forward (e.g. `ssh.sh -- -L 8090:localhost:8090`).
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/config.sh"

# Anything before -- is a remote command (default: shell at REMOTE_DIR).
# Anything after -- is forwarded as ssh flags.
remote_cmd=()
ssh_flags=()
seen_dashes=0
for arg in "$@"; do
  if [ "$arg" = "--" ]; then seen_dashes=1; continue; fi
  if [ "$seen_dashes" -eq 0 ]; then remote_cmd+=("$arg"); else ssh_flags+=("$arg"); fi
done

if [ ${#remote_cmd[@]} -eq 0 ]; then
  exec gcloud compute ssh "$GCP_INSTANCE" "${gcloud_args[@]}" \
    ${ssh_flags[@]:+--ssh-flag="${ssh_flags[*]}"} \
    -- -t "cd ${REMOTE_DIR} && exec \$SHELL -l"
else
  exec gcloud compute ssh "$GCP_INSTANCE" "${gcloud_args[@]}" \
    ${ssh_flags[@]:+--ssh-flag="${ssh_flags[*]}"} \
    --command="cd ${REMOTE_DIR} && ${remote_cmd[*]}"
fi
