This repository is a mono repo, containing deployment artifacts, a rust project, and a react/vite project under web.

# Web UI
The web UI is in the web/ directory.
We use pnpm only for all package management and commands. Prefer pnpm exec <command> over running commands from node_modules.
When we work from the root, there is no pnpm/typescript project at the root.
We use shadcn for components, preferring core shadcn standards where possible (use the shadcn skills for more)
When running pnpm commands, don't run pnpm --dir or compound cd commands. Run a cd into the web folder and then followed by the pnpm command.
