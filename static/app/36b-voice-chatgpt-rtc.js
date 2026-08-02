// ── ChatGPT-lane voice: WebRTC glue + voice card ──
//
// Dumb executor for the WASM provider's RTC verbs (create_call /
// apply_answer / stop_mic / close_pc). Every policy decision — when the
// mic stops, when the peer connection closes, the drain grace for the
// final usage flush, signaling-loss handling — lives in the natively
// tested Rust call state machine; this file only performs browser API
// calls and reports events back via app.chatgpt_rtc_event().
//
// Media flows browser⇄provider. The daemon relays SDP only and never
// carries audio.

(() => {
  const st = { pc: null, mic: null, dc: null, audioEl: null, tick: null };

  function evt(kind, payload) {
    try {
      if (window.app && app.chatgpt_rtc_event) app.chatgpt_rtc_event(kind, payload || '');
    } catch (e) {
      console.warn('[voice-chatgpt] event dispatch failed', kind, e);
    }
  }

  function startTick() {
    if (!st.tick) st.tick = setInterval(() => evt('tick'), 500);
  }
  function stopTick() {
    if (st.tick) { clearInterval(st.tick); st.tick = null; }
  }

  async function createCall() {
    startTick();
    try {
      st.mic = await navigator.mediaDevices.getUserMedia({
        audio: { channelCount: 1, echoCancellation: true, noiseSuppression: true },
      });
    } catch (e) {
      evt('mic_error', String((e && e.message) || e));
      return;
    }
    const pc = new RTCPeerConnection();
    st.pc = pc;
    for (const track of st.mic.getTracks()) pc.addTrack(track, st.mic);
    const dc = pc.createDataChannel('oai-events');
    st.dc = dc;
    dc.onmessage = (e) => { if (typeof e.data === 'string') evt('dc_event', e.data); };
    pc.ontrack = (e) => {
      if (!st.audioEl) {
        st.audioEl = document.createElement('audio');
        st.audioEl.autoplay = true;
        st.audioEl.style.display = 'none';
        document.body.appendChild(st.audioEl);
      }
      st.audioEl.srcObject = (e.streams && e.streams[0]) || new MediaStream([e.track]);
    };
    pc.onconnectionstatechange = () => {
      if (!st.pc || pc !== st.pc) return;
      if (pc.connectionState === 'connected') evt('pc_connected');
      else if (pc.connectionState === 'failed') evt('pc_terminated', 'failed');
    };
    try {
      const offer = await pc.createOffer();
      await pc.setLocalDescription(offer);
      // The provider endpoint is ice-lite (candidates ride the SDP, no
      // trickle) — wait for gathering, bounded.
      if (pc.iceGatheringState !== 'complete') {
        await new Promise((resolve) => {
          const done = () => {
            if (pc.iceGatheringState === 'complete') {
              pc.removeEventListener('icegatheringstatechange', done);
              resolve();
            }
          };
          pc.addEventListener('icegatheringstatechange', done);
          setTimeout(resolve, 3000);
        });
      }
      evt('offer_ready', (pc.localDescription && pc.localDescription.sdp) || '');
    } catch (e) {
      evt('pc_terminated', 'offer: ' + String((e && e.message) || e));
    }
  }

  function stopMicTracks() {
    if (st.mic) {
      for (const t of st.mic.getTracks()) t.stop();
      st.mic = null;
    }
  }

  function closePc() {
    try { if (st.dc) st.dc.close(); } catch (_) {}
    try { if (st.pc) st.pc.close(); } catch (_) {}
    st.dc = null;
    st.pc = null;
    if (st.audioEl) st.audioEl.srcObject = null;
    stopMicTracks();
    stopTick();
  }

  window.voiceChatGptExec = function (cmdJson) {
    let cmd;
    try { cmd = JSON.parse(cmdJson); } catch (_) { return; }
    switch (cmd.kind) {
      case 'create_call':
        createCall();
        break;
      case 'apply_answer':
        if (st.pc) {
          st.pc
            .setRemoteDescription({ type: 'answer', sdp: cmd.sdp })
            .catch((e) => evt('pc_terminated', 'answer: ' + String((e && e.message) || e)));
        }
        break;
      case 'stop_mic':
        stopMicTracks();
        break;
      case 'close_pc':
        closePc();
        break;
      default:
        break;
    }
  };

  // ── Voice card (minimal): resolved pins + provider-reported usage +
  // the owner's thread purge lever. Renders into the ui2 voice panel
  // when present.
  window.voiceCardUpdate = function (statusJson) {
    let status;
    try { status = JSON.parse(statusJson); } catch (_) { return; }
    const host = document.getElementById('ui2-vp-note') || document.getElementById('voiceStatus');
    if (!host) return;
    let card = document.getElementById('vp-chatgpt-card');
    if (!card) {
      card = document.createElement('div');
      card.id = 'vp-chatgpt-card';
      card.className = 'vp-chatgpt-card';
      host.parentNode.insertBefore(card, host.nextSibling);
    }
    const esc = (s) => String(s == null ? '' : s).replace(/[&<>"]/g, (c) => (
      { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]
    ));
    const usage = status.last_usage && status.last_usage.tokens
      ? `tokens ${esc(JSON.stringify(status.last_usage.tokens))} (provider-reported)`
      : '';
    card.innerHTML = [
      `<div class="vp-cg-row">ChatGPT voice · ${status.active ? 'live' : 'idle'}${status.available ? '' : ' · not configured'}</div>`,
      status.resolved_model
        ? `<div class="vp-cg-row">backing ${esc(status.resolved_model)}${status.resolved_effort ? ' @ ' + esc(status.resolved_effort) : ''}</div>`
        : '',
      `<div class="vp-cg-row">${esc(status.realtime_version || 'v3')}${status.voice ? ' · voice ' + esc(status.voice) : ''}${status.thread_id ? ' · thread ' + esc(String(status.thread_id).slice(0, 8)) : ''}${status.thread_lineage_count ? ' · lineage ' + status.thread_lineage_count : ''}</div>`,
      usage ? `<div class="vp-cg-row vp-cg-usage">${usage}</div>` : '',
      status.last_error ? `<div class="vp-cg-row vp-cg-err">${esc(status.last_error)}</div>` : '',
      `<div class="vp-cg-row"><button id="vp-cg-purge" class="vp-cg-purge" title="Delete the durable presence thread (provider-side) and reset its local identity">Purge presence thread</button></div>`,
    ].join('');
    const purge = card.querySelector('#vp-cg-purge');
    if (purge) {
      purge.addEventListener('click', () => {
        if (!window.app || !app.voice_thread_purge) return;
        if (window.confirm('Delete the durable voice presence thread? The next call starts fresh from the checkpoint.')) {
          app.voice_thread_purge();
        }
      });
    }
  };
})();
