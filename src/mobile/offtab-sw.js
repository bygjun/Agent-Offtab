self.addEventListener('push', event => {
  let id;
  try { id = event.data?.json().id; } catch {}
  const session = typeof id === 'string' && /^[\w-]{22}$/.test(id) ? id : undefined;
  event.waitUntil(self.registration.showNotification('Offtab', {
    body: '입력이나 승인을 기다리는 세션이 있습니다.',
    tag: session ?? 'offtab-waiting',
    icon: '/offtab.png',
    data: session ? '/?waiting=' + session : '/',
  }));
});

self.addEventListener('notificationclick', event => {
  event.notification.close();
  const path = event.notification.data;
  const destination = typeof path === 'string' && /^\/(?:\?waiting=[\w-]{22})?$/.test(path) ? path : '/';
  event.waitUntil(self.clients.openWindow(destination));
});
