// Service worker: lets the app be installed, opens fast offline-ish, and handles notification taps.
/// <reference lib="webworker" />

const sw = self as unknown as ServiceWorkerGlobalScope;
declare const PRECACHE: string[];
declare const VERSION: string;

const CACHE = `celerity-${VERSION}`;

sw.addEventListener("install", (e) => {
  e.waitUntil(caches.open(CACHE).then((c) => c.addAll(PRECACHE)).then(() => sw.skipWaiting()));
});

sw.addEventListener("activate", (e) => {
  e.waitUntil(
    caches
      .keys()
      .then((keys) => Promise.all(keys.filter((k) => k.startsWith("celerity-") && k !== CACHE).map((k) => caches.delete(k))))
      .then(() => sw.clients.claim()),
  );
});

sw.addEventListener("fetch", (e) => {
  const url = new URL(e.request.url);
  if (e.request.method !== "GET" || url.origin !== location.origin) return;
  // Never cache the API, files or the socket.
  if (url.pathname.startsWith("/api/") || url.pathname === "/ws") return;
  if (url.pathname.startsWith("/assets/")) {
    // Hashed names never change: cache first.
    e.respondWith(
      caches.match(e.request).then(
        (hit) =>
          hit ??
          fetch(e.request).then((res) => {
            if (res.ok) {
              const copy = res.clone();
              void caches.open(CACHE).then((c) => c.put(e.request, copy));
            }
            return res;
          }),
      ),
    );
    return;
  }
  // The page and icons: network first so updates show up, cache when offline.
  e.respondWith(
    fetch(e.request)
      .then((res) => {
        if (res.ok) {
          const copy = res.clone();
          void caches.open(CACHE).then((c) => c.put(e.request, copy));
        }
        return res;
      })
      .catch(async () => (await caches.match(e.request)) ?? (await caches.match("/"))!),
  );
});

sw.addEventListener("notificationclick", (e) => {
  e.notification.close();
  const channel = (e.notification.data as { channel?: number } | null)?.channel;
  e.waitUntil(
    (async () => {
      const wins = await sw.clients.matchAll({ type: "window", includeUncontrolled: true });
      const win = wins[0] as WindowClient | undefined;
      if (win) {
        await win.focus();
        if (channel != null) win.postMessage({ channel });
      } else {
        if (channel != null) {
          // The app opens this channel first when it starts.
          await sw.clients.openWindow(`/?channel=${channel}`);
        } else {
          await sw.clients.openWindow("/");
        }
      }
    })(),
  );
});
export {};
