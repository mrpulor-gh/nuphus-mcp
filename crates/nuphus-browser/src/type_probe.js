// Run with the resolved input as `this`, in its own execution context (including
// cross-origin frames). A terminal may consume and clear its textarea immediately;
// receipt of trusted input is evidence of delivery, not of command execution.
function(clear, finish) {
    const el = this;
    if (finish) {
        const state = el.__nuphus_type_probe;
        if (!state) throw new Error('Input verification state unavailable; inspect before retrying');
        const received = state.received;
        state.cleanup();
        return received;
    }
    if (el.__nuphus_type_probe) el.__nuphus_type_probe.cleanup();
    const doc = el.ownerDocument;
    const win = doc.defaultView;
    const native = el.tagName === 'INPUT' || el.tagName === 'TEXTAREA';
    if (!native && !el.isContentEditable) throw new Error('Target is not an editable text control');
    if (el.disabled || el.matches(':disabled') || el.readOnly || el.closest('[inert]'))
        throw new Error('Target is disabled, read-only or inert');
    if (el.tagName === 'INPUT' && !['text','search','url','tel','email','password','number'].includes(el.type))
        throw new Error('Target input type does not accept text');
    const r = el.getBoundingClientRect();
    const style = win.getComputedStyle(el);
    if (!el.isConnected || !r.width || !r.height || style.display === 'none' || style.visibility === 'hidden')
        throw new Error('Target is detached or hidden');
    el.scrollIntoViewIfNeeded();
    el.focus();
    const root = el.getRootNode();
    if (root.activeElement !== el) throw new Error('Target could not receive focus');
    if (clear) {
        // Preserve the historical replacement default, but honor clear:false.
        if (native) el.value = '';
        else {
            const range = doc.createRange();
            range.selectNodeContents(el);
            const selection = root.getSelection ? root.getSelection() : win.getSelection();
            selection.removeAllRanges();
            selection.addRange(range);
        }
    }
    const state = {received: false};
    const listener = event => {
        if (event.isTrusted && event.composedPath().includes(el)) state.received = true;
    };
    // Capture before application handlers reset the textarea or replace the node.
    win.addEventListener('input', listener, true);
    state.cleanup = () => {
        win.removeEventListener('input', listener, true);
        win.clearTimeout(state.timer);
        delete el.__nuphus_type_probe;
    };
    state.timer = win.setTimeout(state.cleanup, 10000);
    el.__nuphus_type_probe = state;
    return true;
}
