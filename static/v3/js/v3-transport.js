/* V3 — transport: the two lanes a plain page needs.
   HTTP JSON via fetch, events + ControlMsgs over one /ws text socket.
   Auth is transport-contextual (loopback/mTLS root session) — no token
   handling here beyond the optional federation bearer the V2 SPA also
   honors (localStorage["intendantFederationBearerToken"], appended as
   ?token= on the WS URL when present).
   Wire facts (verified against the daemon):
   - server→client: {"event":"<snake>", ...} OutboundEvents plus
     {"t":"<type>", ...} frames (state_snapshot, log_replay, ws_denied…)
   - client→server: flat ControlMsg JSON, {"action":"<snake>", ...}
   - no subscribe handshake: a fresh socket gets the full bootstrap
     (state_snapshot → cached events → session replays → log_replay)
     before live events flow. */
window.V3 = window.V3 || {};

V3.transport = (function () {
  const handlers = {};   // '<event|t>:<tag>' → [fn]; '*' wildcard on 'event:*'
  let ws = null;
  let booted = false;
  let reconnectDelay = 800;
  let tabId = null;

  const T = {
    state: 'connecting',           // connecting | live | offline
    config: null,
    connectionId: null,
    bootstrapped: false,

    on(tag, fn) { (handlers[tag] = handlers[tag] || []).push(fn); },
    emit(tag, msg) {
      (handlers[tag] || []).forEach(fn => { try { fn(msg); } catch (e) { console.error('[v3] handler ' + tag, e); } });
    },

    async boot() {
      tabId = sessionStorage.getItem('intendant.v3.tab') ||
        (() => { const id = 'v3-' + Math.random().toString(36).slice(2, 10); sessionStorage.setItem('intendant.v3.tab', id); return id; })();
      try { T.config = await T.get('/config'); } catch (e) { T.config = {}; }
      T.connect();
      /* resolve once bootstrapped, or after 4s — never trap first paint */
      return new Promise(resolve => {
        const t0 = Date.now();
        const check = setInterval(() => {
          if (T.bootstrapped || Date.now() - t0 > 4000) { clearInterval(check); resolve(); }
        }, 120);
      });
    },

    wsUrl() {
      const proto = location.protocol === 'https:' ? 'wss://' : 'ws://';
      let url = proto + location.host + '/ws?tab=' + encodeURIComponent(tabId);
      const bearer = localStorage.getItem('intendantFederationBearerToken');
      if (bearer) url += '&token=' + encodeURIComponent(bearer);
      return url;
    },

    connect() {
      T.setState('connecting');
      try { ws = new WebSocket(T.wsUrl()); } catch (e) { T.setState('offline'); return; }
      ws.onopen = () => { T.setState('live'); reconnectDelay = 800; };
      ws.onmessage = ev => T.dispatch(ev.data);
      ws.onclose = () => {
        T.setState('offline');
        setTimeout(T.connect, reconnectDelay);
        reconnectDelay = Math.min(reconnectDelay * 1.6, 10000);
      };
      ws.onerror = () => { try { ws.close(); } catch (e) {} };
    },

    setState(s) {
      if (T.state === s) return;
      T.state = s;
      V3.bus.emit('conn');
    },

    dispatch(raw) {
      let msg;
      try { msg = JSON.parse(raw); } catch (e) { return; }
      if (msg.t === 'state_snapshot') {
        T.connectionId = msg.connection_id || null;
        if (msg.config && !T.config) T.config = msg.config;
      }
      if (msg.t) T.emit('t:' + msg.t, msg);
      if (msg.event) {
        T.emit('event:' + msg.event, msg);
        T.emit('event:*', msg);
      }
      /* bootstrap_flushed marks the end of replay — live lane is open */
      if (msg.t === 'bootstrap_flushed' || (msg.event && !T.bootstrapped)) {
        /* the first live/replayed event also counts: the daemon stamps
           replayed events, but any traffic proves the pipe works */
        T.bootstrapped = true;
      }
    },

    send(obj) {
      if (ws && ws.readyState === 1) { ws.send(JSON.stringify(obj)); return true; }
      V3.toast('The line to the house is down — reconnecting…', 'brick');
      return false;
    },

    async get(path) {
      const r = await fetch(path, { headers: T.authHeaders() });
      if (!r.ok) throw new Error('GET ' + path + ' → ' + r.status);
      return r.json();
    },
    async post(path, body) {
      const r = await fetch(path, {
        method: 'POST',
        headers: Object.assign({ 'Content-Type': 'application/json' }, T.authHeaders()),
        body: JSON.stringify(body)
      });
      if (!r.ok) {
        const text = await r.text().catch(() => '');
        throw new Error('POST ' + path + ' → ' + r.status + (text ? ' · ' + text.slice(0, 160) : ''));
      }
      const ct = r.headers.get('content-type') || '';
      return ct.includes('json') ? r.json() : r.text();
    },
    authHeaders() {
      const bearer = localStorage.getItem('intendantFederationBearerToken');
      return bearer ? { 'Authorization': 'Bearer ' + bearer } : {};
    }
  };
  return T;
})();
