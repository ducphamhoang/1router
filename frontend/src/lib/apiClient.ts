const CSRF_HEADER = "X-Requested-With";
const CSRF_VALUE = "1router-ui";

type ApiErrorBody = {
  error?: {
    message?: string;
  };
};

type ApiFetchInit = RequestInit & {
  skipAuthRedirect?: boolean;
};

export async function apiFetch(path: string, init: ApiFetchInit = {}) {
  const { skipAuthRedirect, ...requestInit } = init;
  const method = (init.method ?? "GET").toUpperCase();
  const headers = new Headers(requestInit.headers);

  if (method !== "GET") {
    headers.set(CSRF_HEADER, CSRF_VALUE);
  }

  const response = await fetch(path, {
    ...requestInit,
    method,
    headers,
    credentials: "include"
  });

  if (response.status === 401 && !skipAuthRedirect && window.location.pathname !== "/ui/login") {
    window.location.assign("/ui/login");
  }

  return response;
}

export async function apiJson<T>(path: string, init: ApiFetchInit = {}): Promise<T> {
  const response = await apiFetch(path, init);
  const text = await response.text();
  const body = text ? (JSON.parse(text) as ApiErrorBody | T) : undefined;

  if (!response.ok) {
    const message =
      (body as ApiErrorBody | undefined)?.error?.message ?? `Request failed with status ${response.status}`;
    throw new Error(message);
  }

  return body as T;
}
