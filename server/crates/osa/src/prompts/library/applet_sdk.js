
(function () {
  if (window.osa) return;
  var n = 0, waits = {}, handlers = {};
  function rpc(method, params) {
    return new Promise(function (res, rej) {
      var id = ++n;
      waits[id] = { res: res, rej: rej };
      parent.postMessage({ osa: 1, id: id, method: method, params: params || {} }, '*');
      setTimeout(function () { if (waits[id]) { delete waits[id]; rej(new Error('timeout')); } }, 35 * 60 * 1000);
    });
  }
  window.addEventListener('message', function (e) {
    var d = e.data;
    if (!d || d.osa !== 1 || e.source !== parent) return;
    if (d.id && waits[d.id]) {
      var w = waits[d.id]; delete waits[d.id];
      d.error ? w.rej(new Error(d.error)) : w.res(d.result);
    } else if (d.event) {
      (handlers[d.event] || []).forEach(function (f) { try { f(d.data); } catch (err) { console.error(err); } });
    }
  });
  function height() { var b = document.body, h = document.documentElement; return Math.max(b ? b.scrollHeight : 0, h.scrollHeight, b ? b.offsetHeight : 0); }
  window.osa = {
    call: function (tool, args) { return rpc('call', { tool: tool, args: args || {} }); },
    send: function (text) { return rpc('say', { text: String(text) }); },
    store: {
      get: function (key) { return rpc('data.get', { key: key }); },
      all: function () { return rpc('data.get', {}); },
      set: function (key, value) { var o = {}; o[key] = value; return rpc('data.set', { data: o }); },
      remove: function (key) { var o = {}; o[key] = null; return rpc('data.set', { data: o }); }
    },
    on: function (event, fn) { (handlers[event] = handlers[event] || []).push(fn); },
    resize: function (h) { parent.postMessage({ osa: 1, event: 'resize', height: h == null ? height() : h }, '*'); }
  };
  function start() {
    parent.postMessage({ osa: 1, event: 'ready' }, '*');
    window.osa.resize();
    if (window.ResizeObserver) {
      var ro = new ResizeObserver(function () { window.osa.resize(); });
      ro.observe(document.documentElement);
      if (document.body) ro.observe(document.body);
    }
    setInterval(function () { window.osa.resize(); }, 1500);
  }
  document.readyState === 'loading' ? document.addEventListener('DOMContentLoaded', start) : start();
})();
