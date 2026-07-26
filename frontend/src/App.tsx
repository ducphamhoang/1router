import { Navigate, NavLink, Route, Routes } from "react-router-dom";
import { Login } from "./pages/Login";
import { Pools } from "./pages/Pools";
import { Providers } from "./pages/Providers";
import { Settings } from "./pages/Settings";

export function App() {
  return (
    <main>
      <nav aria-label="Admin sections">
        <NavLink to="/ui/providers">Providers</NavLink>
        <NavLink to="/ui/pools">Pools</NavLink>
        <NavLink to="/ui/settings">Settings</NavLink>
      </nav>
      <Routes>
        <Route path="/ui/login" element={<Login />} />
        <Route path="/ui/providers" element={<Providers />} />
        <Route path="/ui/pools" element={<Pools />} />
        <Route path="/ui/settings" element={<Settings />} />
        <Route path="*" element={<Navigate to="/ui/providers" replace />} />
      </Routes>
    </main>
  );
}
