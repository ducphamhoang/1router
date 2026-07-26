import { Navigate, NavLink, Route, Routes } from "react-router-dom";
import { Login } from "./pages/Login";
import { Providers } from "./pages/Providers";

function Placeholder({ title }: { title: string }) {
  return <h1>{title}</h1>;
}

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
        <Route path="/ui/pools" element={<Placeholder title="Pools" />} />
        <Route path="/ui/settings" element={<Placeholder title="Settings" />} />
        <Route path="*" element={<Navigate to="/ui/providers" replace />} />
      </Routes>
    </main>
  );
}
