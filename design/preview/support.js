// Minimal local renderer for .dc.html design files (screenshots only).
(function () {
  const HOLE = /\{\{\s*([^}]+?)\s*\}\}/g;
  let tpl, root, comp, pending = false, mounted = false;
  class DCLogic {
    constructor(props) { this.props = props || {}; this.state = {}; }
    setState(p) { this.state = Object.assign({}, this.state, typeof p === 'function' ? p(this.state) : p); schedule(); }
    forceUpdate() { schedule(); }
  }
  window.DCLogic = DCLogic;
  function schedule() { if (pending) return; pending = true; requestAnimationFrame(() => { pending = false; render(); }); }
  function lookup(scope, path) {
    path = path.trim();
    if (path === 'true') return true;
    if (path === 'false') return false;
    if (/^-?\d+(\.\d+)?$/.test(path)) return Number(path);
    const parts = path.split('.');
    for (let i = scope.length - 1; i >= 0; i--) {
      if (scope[i] && parts[0] in scope[i]) {
        let v = scope[i][parts[0]];
        for (const p of parts.slice(1)) v = v == null ? undefined : v[p];
        return v;
      }
    }
    return undefined;
  }
  function interp(str, scope) {
    const m = str.match(/^\{\{\s*([^}]+?)\s*\}\}$/);
    if (m) return lookup(scope, m[1]);
    return str.replace(HOLE, (_, p) => { const v = lookup(scope, p); return v == null ? '' : String(v); });
  }
  function proc(node, scope, out) {
    if (node.nodeType === 3) { const v = interp(node.nodeValue, scope); out.appendChild(document.createTextNode(v == null ? '' : String(v))); return; }
    if (node.nodeType !== 1) return;
    const tag = node.tagName.toLowerCase();
    if (tag === 'sc-for') {
      const list = interp(node.getAttribute('list') || '', scope) || [];
      const as = node.getAttribute('as') || 'item';
      list.forEach((it, idx) => { const sc = scope.concat([{ [as]: it, $index: idx }]); node.childNodes.forEach((c) => proc(c, sc, out)); });
      return;
    }
    if (tag === 'sc-if') { if (interp(node.getAttribute('value') || '', scope)) node.childNodes.forEach((c) => proc(c, scope, out)); return; }
    if (tag === 'helmet') return;
    const svg = node.namespaceURI === 'http://www.w3.org/2000/svg';
    const el = svg ? document.createElementNS(node.namespaceURI, node.tagName) : document.createElement(node.tagName);
    for (const a of Array.from(node.attributes)) {
      const name = a.name;
      if (/^on[a-z]+$/i.test(name)) {
        const fn = interp(a.value, scope);
        if (typeof fn === 'function') el.addEventListener(name.slice(2).toLowerCase(), fn);
        continue;
      }
      const v = interp(a.value, scope);
      if (name === 'checked' || name === 'disabled' || name === 'selected') {
        if (v === true || v === 'true' || v === '' ) { el.setAttribute(name, ''); if (name === 'checked') el.checked = true; }
        continue;
      }
      if (name.indexOf('hint-') === 0) continue;
      el.setAttribute(name, v == null ? '' : String(v));
      if (name === 'value' && el.tagName === 'INPUT') el.value = v;
    }
    node.childNodes.forEach((c) => proc(c, scope, el));
    out.appendChild(el);
  }
  function render() {
    const vals = comp.renderVals();
    const frag = document.createDocumentFragment();
    tpl.childNodes.forEach((c) => proc(c, [vals], frag));
    root.replaceChildren(frag);
    if (!mounted) { mounted = true; if (comp.componentDidMount) comp.componentDidMount(); }
    window.__dcRenders = (window.__dcRenders || 0) + 1;
  }
  document.addEventListener('DOMContentLoaded', () => {
    tpl = document.querySelector('x-dc');
    const helmet = tpl.querySelector('helmet');
    if (helmet) helmet.querySelectorAll('style, link').forEach((n) => document.head.appendChild(n));
    root = document.createElement('div');
    tpl.parentNode.insertBefore(root, tpl);
    tpl.remove();
    const s = document.querySelector('script[data-dc-script]');
    const props = {};
    try { const dp = JSON.parse(s.getAttribute('data-props') || '{}'); Object.keys(dp).forEach((k) => { if (k[0] !== '$' && dp[k] && 'default' in dp[k]) props[k] = dp[k].default; }); } catch (e) { console.error(e); }
    const C = new Function('DCLogic', s.textContent + '\n;return Component;')(DCLogic);
    comp = new C(props);
    render();
  });
})();
