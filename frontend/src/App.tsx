import { useEffect, useState } from "react";
import { Navigate, NavLink, Route, Routes, useLocation } from "react-router-dom";
import { apiJson } from "./lib/apiClient";
import { Login } from "./pages/Login";
import { Pools } from "./pages/Pools";
import { Providers } from "./pages/Providers";
import { Settings } from "./pages/Settings";

type SecurityStatus = {
  shared_secret_is_default: boolean;
  admin_password_is_default: boolean;
  require_shared_secret: boolean;
  listen_addr_is_loopback: boolean;
};

// Nudges an operator still on the onboarding fast-path defaults (see
// core::config::DEFAULT_SHARED_SECRET/DEFAULT_ADMIN_PASSWORD) to rotate
// them before exposing this instance beyond localhost. Shown on every admin
// page except Login, which isn't authenticated yet.
export function SecurityBanner() {
  const location = useLocation();
  const [status, setStatus] = useState<SecurityStatus | null>(null);

  useEffect(() => {
    if (location.pathname === "/ui/login") {
      setStatus(null);
      return;
    }
    let cancelled = false;
    apiJson<SecurityStatus>("/admin/settings/security-status", { skipAuthRedirect: true })
      .then((body) => {
        if (!cancelled) {
          setStatus(body);
        }
      })
      .catch(() => {
        // Not logged in yet, or the request otherwise failed - say nothing
        // rather than nag on top of whatever the page itself is showing.
        if (!cancelled) {
          setStatus(null);
        }
      });
    return () => {
      cancelled = true;
    };
  }, [location.pathname]);

  if (
    !status ||
    (status.require_shared_secret && !status.shared_secret_is_default && !status.admin_password_is_default)
  ) {
    return null;
  }

  const openAccessWarning = !status.require_shared_secret;
  const criticalOpenAccess = openAccessWarning && !status.listen_addr_is_loopback;

  const warnings: string[] = [];
  if (status.admin_password_is_default) {
    warnings.push("the admin UI password is still the published default ('password')");
  }
  if (status.shared_secret_is_default) {
    warnings.push("the shared API secret is still the published default ('1router-api-key')");
  }

  return (
    <>
      {openAccessWarning ? (
        <p role="alert" className={criticalOpenAccess ? "security-critical" : "security-warning"}>
          {criticalOpenAccess
            ? "Open access is on and this gateway isn't bound to localhost — anyone who can reach it can send requests through your providers with no credentials. Restrict ROUTER_LISTEN_ADDR to 127.0.0.1 or require an API key on the Settings page."
            : "Open access is on: /v1/* accepts requests with no API key. Change this on the Settings page."}
        </p>
      ) : null}
      {warnings.length > 0 ? (
        <p role="alert">
          Still using onboarding defaults: {warnings.join(" and ")} — change{" "}
          {warnings.length > 1 ? "them" : "it"} on the{" "}
          <NavLink to="/ui/settings">Settings</NavLink> page (or via <code>1router setup</code>)
          before exposing this instance beyond localhost.
        </p>
      ) : null}
    </>
  );
}

export function App() {
  return (
    <main>
      <nav aria-label="Admin sections">
        <NavLink to="/ui/providers">Providers</NavLink>
        <NavLink to="/ui/pools">Pools</NavLink>
        <NavLink to="/ui/settings">Settings</NavLink>
      </nav>
      <SecurityBanner />
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
