// Feature tour tabs, copy buttons, promo video dialog, top bar border.
(function () {
  var tabs = Array.prototype.slice.call(document.querySelectorAll('.tour-tab'));
  var desc = document.querySelector('.tour-desc');
  function select(tab, focus) {
    tabs.forEach(function (t) {
      var on = t === tab;
      t.setAttribute('aria-selected', on ? 'true' : 'false');
      t.tabIndex = on ? 0 : -1;
      var panel = document.getElementById(t.getAttribute('aria-controls'));
      if (panel) panel.hidden = !on;
    });
    if (desc) desc.textContent = tab.querySelector('span').textContent;
    if (focus) {
      tab.focus();
      tab.scrollIntoView({ block: 'nearest', inline: 'nearest' });
    }
  }
  tabs.forEach(function (tab, i) {
    tab.addEventListener('click', function () { select(tab, false); });
    tab.addEventListener('keydown', function (e) {
      var next = null;
      if (e.key === 'ArrowDown' || e.key === 'ArrowRight') next = tabs[(i + 1) % tabs.length];
      if (e.key === 'ArrowUp' || e.key === 'ArrowLeft') next = tabs[(i - 1 + tabs.length) % tabs.length];
      if (e.key === 'Home') next = tabs[0];
      if (e.key === 'End') next = tabs[tabs.length - 1];
      if (next) { e.preventDefault(); select(next, true); }
    });
  });
  if (tabs.length) select(tabs[0], false);

  document.querySelectorAll('.copy').forEach(function (btn) {
    var label = btn.textContent;
    btn.addEventListener('click', function () {
      var text = btn.parentElement.querySelector('pre').innerText
        .split('\n').map(function (l) { return l.replace(/\s+#.*$/, ''); }).join('\n').trim();
      var done = function () {
        btn.textContent = btn.dataset.done;
        setTimeout(function () { btn.textContent = label; }, 1600);
      };
      if (navigator.clipboard) navigator.clipboard.writeText(text).then(done, function () {});
    });
  });

  var dialog = document.querySelector('dialog.promo');
  if (dialog) {
    var video = dialog.querySelector('video');
    document.querySelectorAll('[data-open-promo]').forEach(function (b) {
      b.addEventListener('click', function () {
        if (!dialog.showModal) { location.href = video.querySelector('source').src; return; }
        dialog.showModal();
        video.play().catch(function () {});
      });
    });
    dialog.querySelector('.close').addEventListener('click', function () { dialog.close(); });
    dialog.addEventListener('click', function (e) { if (e.target === dialog) dialog.close(); });
    dialog.addEventListener('close', function () { video.pause(); });
  }

  var top = document.querySelector('.top');
  if (top) {
    var onScroll = function () { top.classList.toggle('scrolled', window.scrollY > 8); };
    window.addEventListener('scroll', onScroll, { passive: true });
    onScroll();
  }
})();
