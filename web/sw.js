// ESC service worker: the app shell opens instantly and survives a flaky network.
// It never caches anyone's files or messages.
const CACHE = 'esc-shell';

self.addEventListener('install', () => self.skipWaiting());
self.addEventListener('activate', (e) => e.waitUntil(self.clients.claim()));

self.addEventListener('fetch', (e) => {
  const req = e.request;
  const url = new URL(req.url);
  if (req.method !== 'GET' || url.origin !== location.origin) return;
  if (url.pathname === '/') {
    // Network first, so a new build shows up at once; the cached shell only when offline.
    e.respondWith(
      fetch(req)
        .then((r) => {
          if (r.ok) {
            const copy = r.clone();
            caches.open(CACHE).then((c) => c.put('/', copy));
          }
          return r;
        })
        .catch(() => caches.match('/')),
    );
  } else if (url.pathname.startsWith('/a/') && url.searchParams.has('v')) {
    // Versioned assets never change: cache first.
    e.respondWith(
      caches.match(req).then(
        (hit) =>
          hit ||
          fetch(req).then((r) => {
            if (r.ok) {
              const copy = r.clone();
              caches.open(CACHE).then((c) => c.put(req, copy));
            }
            return r;
          }),
      ),
    );
  }
});
