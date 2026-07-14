// Intendant Connect service worker: shows pushed notifications and opens
// the discovery directory (or another explicitly supplied Connect URL) on
// click. It never opens a daemon dashboard or control session. Payloads arrive
// end-to-end encrypted to this browser's subscription; by the time they reach
// here the browser has already decrypted them.
self.addEventListener('push', event => {
  let data = {};
  try { data = event.data ? event.data.json() : {}; } catch {}
  const title = data.title || 'Intendant Connect';
  event.waitUntil(self.registration.showNotification(title, {
    body: data.body || '',
    data: { url: data.url || '/connect' },
    icon: undefined,
    tag: data.tag || undefined,
  }));
});

self.addEventListener('notificationclick', event => {
  event.notification.close();
  const url = event.notification.data?.url || '/connect';
  event.waitUntil((async () => {
    const all = await clients.matchAll({ type: 'window', includeUncontrolled: true });
    for (const client of all) {
      if (client.url.includes(url) && 'focus' in client) return client.focus();
    }
    return clients.openWindow(url);
  })());
});
