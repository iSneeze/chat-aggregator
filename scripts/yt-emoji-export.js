// ==UserScript==
// @name         YouTube Emoji Picker Exporter
// @namespace    chat-aggregator
// @version      1.6
// @description  Scrape custom emoji (token -> image URL) from YouTube's live chat emoji picker
// @match        https://www.youtube.com/*
// @run-at       document-idle
// @grant        GM_registerMenuCommand
// @grant        GM_setClipboard
// ==/UserScript==

(function () {
  'use strict';
  try {
    console.log('[emoji-exporter] injected:', location.href);

    // Only the top frame shows UI; every frame still captures.
    let isTop = false;
    try { isTop = window.top === window; } catch (_) {}

    // Custom/channel emote images live on yt3.ggpht.com. The standard unicode
    // emoji categories use other hosts — and we don't need them anyway,
    // because unicode emoji arrive as characters, not tokens.
    const GGPHT = /yt3\.ggpht\.com/;
    const CATEGORY_TAGS =
      'yt-emoji-picker-category-renderer, yt-emoji-picker-upsell-category-renderer';

    const seen = new Map();
    const keptCategories = new Set();
    let countEl = null; // set only in the top frame

    function chatDocs() {
      const docs = [document];
      for (const f of document.querySelectorAll('iframe')) {
        try { if (f.contentDocument) docs.push(f.contentDocument); } catch (_) {}
      }
      return docs;
    }

    function categoryName(r) {
      return (r.querySelector('#category-name, #title, yt-formatted-string')?.textContent || '').trim();
    }

    function keepCategory(r, isUpsell) {
      if (isUpsell) return true;                       // locked channel emotes
      if (/you\s*tube/i.test(categoryName(r))) return true;  // global custom set
      const imgs = [...r.querySelectorAll('img[role="option"]')];
      return imgs.length > 0 && imgs.every(i => GGPHT.test(i.getAttribute('src') || ''));
    }

    function scrape() {
      for (const doc of chatDocs()) {
        let nodes;
        try { nodes = [...doc.querySelectorAll(CATEGORY_TAGS)]; } catch (_) { continue; }

        for (const r of nodes) {
          const isUpsell = r.tagName.toLowerCase() === 'yt-emoji-picker-upsell-category-renderer';
          if (!keepCategory(r, isUpsell)) continue;
          const cat = categoryName(r);
          keptCategories.add(isUpsell ? `${cat} (locked)` : cat);

          for (const img of r.querySelectorAll('img[role="option"]')) {
            const code = img.getAttribute('aria-label') || '';
            const url  = img.getAttribute('src') || '';
            const id   = img.getAttribute('id') || '';
            if (!code.startsWith(':') || !code.endsWith(':') || !url) continue;
            if (seen.has(code)) continue;
            const slash = id.indexOf('/');
            const isChannel = isUpsell || slash > 0;
            seen.set(code, {
              code, match: 'token', url, emoji_id: id, category: cat,
              source: isChannel ? 'youtube-channel' : 'youtube-global',
              channel_id: slash > 0 ? id.slice(0, slash) : null,
            });
          }
        }

        // Export control inside the picker's category toolbar.
        const bar = doc.querySelector('#category-buttons');
        if (bar && !bar.querySelector('.ytee-export')) {
          const b = doc.createElement('button');
          b.className = 'ytee-export';
          b.textContent = '⤓';
          b.title = 'Export captured emoji as JSON';
          b.style.cssText = 'cursor:pointer;margin-left:6px;font-size:16px';
          b.addEventListener('click', e => { e.stopPropagation(); e.preventDefault(); download(); });
          bar.appendChild(b);
        }
      }
      if (countEl) countEl.textContent = String(seen.size);
    }

    async function sweep() {
      for (const doc of chatDocs()) {
        const sc = doc.querySelector('yt-emoji-picker-renderer #categories')
                || doc.querySelector('#categories')
                || [...doc.querySelectorAll(CATEGORY_TAGS)].map(e => e.parentElement).find(Boolean);
        if (!sc) continue;
        const step = sc.clientHeight || 200;
        let last = -1;
        for (let i = 0; i < 400 && sc.scrollTop !== last; i++) {
          last = sc.scrollTop;
          sc.scrollTop = Math.min(last + step, sc.scrollHeight);
          await new Promise(r => setTimeout(r, 150));
          scrape();
        }
      }
    }

    function json() {
      const cid = [...seen.values()].find(e => e.channel_id)?.channel_id || null;
      return JSON.stringify({
        version: 1,
        generated_at: new Date().toISOString(),
        source: 'youtube-emoji-picker',
        channel_id: cid,
        categories: [...keptCategories],
        count: seen.size,
        entries: [...seen.values()],
      }, null, 2);
    }

    function report() {
      console.log('[emoji-exporter] kept categories:', [...keptCategories].join(' | ') || '(none)');
      console.log('[emoji-exporter] total:', seen.size);
    }

    function download() {
      if (!seen.size) { alert('No emoji captured — open the emoji picker first.'); return; }
      report();
      const cid = [...seen.values()].find(e => e.channel_id)?.channel_id;
      const a = document.createElement('a');
      a.href = URL.createObjectURL(new Blob([json()], { type: 'application/json' }));
      a.download = `youtube-emojis${cid ? '-' + cid : ''}.json`;
      a.click();
      URL.revokeObjectURL(a.href);
    }

    function copy() {
      if (!seen.size) { alert('No emoji captured — open the emoji picker first.'); return; }
      report();
      const s = json();
      if (typeof GM_setClipboard === 'function') GM_setClipboard(s);
      else navigator.clipboard.writeText(s);
    }

    // ---- UI: top frame only ----
    if (isTop) {
      const panel = document.createElement('div');
      panel.id = 'ytee-panel';
      panel.style.cssText =
        'position:fixed;z-index:2147483647;left:12px;top:12px;background:#111;color:#eee;' +
        'font:12px/1.5 ui-monospace,monospace;padding:8px 10px;border-radius:8px;' +
        'border:1px solid #e33;opacity:.95;user-select:none';

      // DOM APIs only: YouTube enforces Trusted Types, so innerHTML throws.
      const line = document.createElement('div');
      line.append('emoji: ');
      countEl = document.createElement('b');
      countEl.textContent = '0';
      line.appendChild(countEl);
      panel.appendChild(line);

      for (const [label, fn] of [['Sweep', sweep], ['Download', download], ['Copy', copy]]) {
        const b = document.createElement('button');
        b.textContent = label;
        b.style.cssText = 'margin:6px 6px 0 0';
        b.addEventListener('click', fn);
        panel.appendChild(b);
      }

      let drag = false, ox = 0, oy = 0;
      panel.addEventListener('mousedown', e => {
        if (e.target.tagName === 'BUTTON') return;
        drag = true; ox = e.clientX - panel.offsetLeft; oy = e.clientY - panel.offsetTop;
      });
      addEventListener('mousemove', e => {
        if (!drag) return;
        panel.style.left = (e.clientX - ox) + 'px';
        panel.style.top  = (e.clientY - oy) + 'px';
      });
      addEventListener('mouseup', () => { drag = false; });

      document.documentElement.appendChild(panel);
      console.log('[emoji-exporter] panel added to', location.href);

      const reg = typeof GM_registerMenuCommand === 'function' ? GM_registerMenuCommand
        : (typeof GM !== 'undefined' && GM.registerMenuCommand) ? GM.registerMenuCommand.bind(GM) : null;
      if (reg) {
        reg('YT emoji: sweep picker', sweep);
        reg('YT emoji: download JSON', download);
        reg('YT emoji: copy JSON', copy);
      }
    }

    setInterval(scrape, 1000);
    scrape();
  } catch (e) {
    console.error('[emoji-exporter] fatal:', e);
  }
})();
