import { Outlet } from 'react-router-dom';
import { NavSpine } from '../components/NavSpine';

// The shell shared by every surface: the sticky nav spine on top, the
// active surface in the middle (via <Outlet/>), a quiet footer at the
// foot. The masthead mark is the static brand logo; per-session boot
// loaders carry the "something is happening" signal where it belongs.

export function Layout() {
  return (
    <>
      <NavSpine />
      <Outlet />
      <Footer />
    </>
  );
}

function Footer() {
  return (
    <footer className="book-wide mt-20 mb-4">
      <hr />
    </footer>
  );
}
