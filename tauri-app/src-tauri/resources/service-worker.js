const CACHE='micyou-web-v2';
const STATIC=['/manifest.webmanifest','/alpine.min.js'];

self.addEventListener('install',event=>{
  self.skipWaiting();
  event.waitUntil(caches.open(CACHE).then(cache=>cache.addAll(STATIC)).catch(()=>{}));
});

self.addEventListener('activate',event=>{
  event.waitUntil(Promise.all([
    caches.keys().then(keys=>Promise.all(keys.filter(k=>k!==CACHE).map(k=>caches.delete(k)))),
    self.clients.claim()
  ]));
});

self.addEventListener('fetch',event=>{
  const request=event.request;
  if(request.method!=='GET') return;
  const url=new URL(request.url);
  if(request.mode==='navigate'||url.pathname.startsWith('/api/')||url.pathname==='/ws') return;
  if(!STATIC.includes(url.pathname)) return;
  event.respondWith(caches.match(request).then(hit=>hit||fetch(request).then(response=>{
    if(response.ok){const copy=response.clone();caches.open(CACHE).then(cache=>cache.put(request,copy)).catch(()=>{});}
    return response;
  })));
});
