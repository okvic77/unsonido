/** Thin fetch wrapper for the unsonido API, with the access token in localStorage. */

const TOKEN_KEY = "unsonido-token";

export function loadToken(): string {
  try {
    return localStorage.getItem(TOKEN_KEY) || "";
  } catch {
    return "";
  }
}

export function saveToken(token: string) {
  try {
    if (token) localStorage.setItem(TOKEN_KEY, token);
    else localStorage.removeItem(TOKEN_KEY);
  } catch {
    // localStorage can be unavailable (private mode); the token still works in-session.
  }
}

export class ApiError extends Error {
  constructor(
    readonly status: number,
    message: string,
  ) {
    super(message);
  }
}

const token = { value: loadToken() };

export function setToken(next: string) {
  token.value = next;
  saveToken(next);
}

export function hasToken() {
  return !!token.value;
}

export async function api<T = unknown>(
  method: string,
  path: string,
  body?: unknown,
): Promise<T> {
  const headers: Record<string, string> = {};
  if (body !== undefined) headers["content-type"] = "application/json";
  if (token.value) headers.authorization = "Bearer " + token.value;
  const res = await fetch(path, {
    method,
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await res.text();
  const data = text ? JSON.parse(text) : null;
  if (!res.ok) {
    const message =
      (data && typeof data === "object" && "error" in data
        ? String(data.error)
        : null) ?? res.statusText;
    throw new ApiError(res.status, message);
  }
  return data as T;
}