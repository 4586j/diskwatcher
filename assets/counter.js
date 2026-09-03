document.querySelectorAll('[data-count]').forEach(el => {
  const target = parseFloat(el.dataset.count) || 0;
  const isBytes = el.dataset.fmt === 'bytes';
  const human = n => {
    const u = ['B', 'KB', 'MB', 'GB', 'TB'];
    let i = 0, v = n;
    while (v >= 1024 && i < u.length - 1) { v /= 1024; i++; }
    return i === 0 ? Math.round(n) + ' B' : v.toFixed(1) + ' ' + u[i];
  };
  const t0 = performance.now(), dur = 900;
  const finish = () => { el.textContent = isBytes ? human(target) : Math.round(target).toLocaleString(); };
  // 后台标签页里 rAF 不跑，动画会把数字永远停在占位的 0。
  if (document.hidden) { finish(); return; }
  const step = t => {
    if (document.hidden) { finish(); return; }
    const p = Math.max(0, Math.min(1, (t - t0) / dur));
    const e = 1 - Math.pow(1 - p, 3);
    const v = target * e;
    el.textContent = isBytes ? human(v) : Math.round(v).toLocaleString();
    if (p < 1) requestAnimationFrame(step);
  };
  requestAnimationFrame(step);
});
