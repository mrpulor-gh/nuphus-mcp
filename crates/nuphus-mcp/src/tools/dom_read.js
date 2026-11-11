// Read public DOM text once. Match before truncating the returned tail.
function nuphusDomRead(options) {
    const root = this.closest('.xterm') || this.ownerDocument;
    const element = root.querySelector(options.selector);
    if (!element) throw new Error('DOM text target not found');
    const text = element.innerText || element.textContent || '';
    const characters = Array.from(text);
    const value = {
        result: characters.slice(-options.max_chars).join(''),
        total_chars: characters.length,
        truncated: characters.length > options.max_chars,
        matched: !options.until_text || text.includes(options.until_text)
    };
    if (options.completion_prefix) {
        // A trailing newline is required: a partially painted numeric exit code
        // (e.g. the first digit of 13) is not yet a complete receipt.
        const lines = text.split('\n');
        lines.pop();
        const receipts = lines.map(line => line.replace(/\r$/, ''))
            .filter(line => line.startsWith(options.completion_prefix))
            .map(line => line.slice(options.completion_prefix.length))
            .filter(code => /^(0|[1-9][0-9]{0,2})$/.test(code) && Number(code) <= 255);
        value.matched = receipts.length === 1;
        if (receipts.length === 1) {
            value.exit_code = Number(receipts[0]);
            value.completion_line = options.completion_prefix + receipts[0];
        } else if (receipts.length > 1) {
            value.ambiguous = true;
        }
    }
    return value;
}
