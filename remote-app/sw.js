// Praxis Remote service worker: caches the app shell only.
// Requests to other origins (notably api.github.com) are never intercepted or cached.

const CACHE = "praxis-remote-v2";
const SHELL = [
  "./",
  "./index.html",
  "./app.js",
  "./styles.css",
  "./manifest.webmanifest",
  "./icon.svg",
  "./icon-192.png",
  "./icon-512.png",
  "./icon-maskable-512.png",
];
const SHELL_URLS = new Set(SHELL.map((path) => new URL(path, self.registration.scope).href));
const INDEX_URL = new URL("./", self.registration.scope).href;

self.addEventListener("install", (event) => {
  event.waitUntil(
    caches
      .open(CACHE)
      .then((cache) => cache.addAll(SHELL))
      .then(() => self.skipWaiting()),
  );
});

self.addEventListener("activate", (event) => {
  event.waitUntil(
    caches
      .keys()
      .then((keys) => Promise.all(keys.filter((key) => key !== CACHE).map((key) => caches.delete(key))))
      .then(() => self.clients.claim()),
  );
});

self.addEventListener("fetch", (event) => {
  const { request } = event;
  if (request.method !== "GET") return;
  const url = new URL(request.url);
  if (url.origin !== self.location.origin) return;

  if (request.mode === "navigate") {
    event.respondWith(networkFirst(request, INDEX_URL));
    return;
  }
  url.search = "";
  if (SHELL_URLS.has(url.href)) {
    event.respondWith(networkFirst(request, url.href));
  }
});

// Network first so updates land immediately when online; the cache is the offline fallback.
async function networkFirst(request, cacheKey) {
  const cache = await caches.open(CACHE);
  try {
    const response = await fetch(request, { cache: "no-cache" });
    if (response.ok && response.type === "basic" && !response.redirected) {
      await cache.put(cacheKey, response.clone());
    }
    return response;
  } catch (error) {
    const cached = (await cache.match(cacheKey)) ?? (await cache.match(INDEX_URL));
    if (cached) return cached;
    throw error;
  }
}
