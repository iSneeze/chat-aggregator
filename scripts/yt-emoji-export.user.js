// ==UserScript==
// @name         YouTube Emoji Picker Exporter
// @namespace    chat-aggregator
// @version      1.8
// @license      MIT
// @homepageURL  https://isneeze.github.io/chat-aggregator/youtube-emoji
// @downloadURL  https://github.com/iSneeze/chat-aggregator/raw/master/scripts/yt-emoji-export.user.js
// @updateURL    https://github.com/iSneeze/chat-aggregator/raw/master/scripts/yt-emoji-export.user.js
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

    // Only the top frame registers menu commands; every frame still captures.
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

        // The only on-page control: an export button inside the picker's
        // category toolbar. It sweeps first, so one click captures every
        // emoji, including the ones the picker hasn't scrolled into view yet.
        const bar = doc.querySelector('#category-buttons');
        if (bar && !bar.querySelector('.ytee-export')) {
          const b = doc.createElement('button');
          b.className = 'ytee-export';
          b.textContent = '⤓';
          b.title = 'Export captured emoji as JSON';
          b.style.cssText = 'cursor:pointer;margin-left:6px;font-size:16px';
          b.addEventListener('click', async e => {
            e.stopPropagation(); e.preventDefault();
            if (b.disabled) return;
            b.disabled = true;
            try { await sweep(); download(); } finally { b.disabled = false; }
          });
          bar.appendChild(b);
        }
      }
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

    // ---- Menu commands (userscript manager menu): top frame only ----
    if (isTop) {
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
