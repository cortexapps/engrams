import { Moon, Sun } from 'lucide-react';
import { useTheme } from './theme-provider';

export function ModeToggle() {
  const { theme, toggle } = useTheme();
  return (
    <button
      type="button"
      aria-label="Toggle theme"
      onClick={toggle}
      className="inline-flex size-8 items-center justify-center rounded-md hover:bg-sidebar-accent"
    >
      {theme === 'dark' ? <Sun className="size-4" /> : <Moon className="size-4" />}
    </button>
  );
}
