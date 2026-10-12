(() => {
  const send = (text) => {
    if (typeof text !== "string" || text.length === 0) return;
    try {
      globalThis.__BINDING__?.(text.slice(0, __LIMIT__));
    } catch {}
  };
  const selection = () => {
    const field = document.activeElement;
    if (
      (field instanceof HTMLInputElement || field instanceof HTMLTextAreaElement) &&
      field.selectionStart !== null
    )
      return field.value.slice(field.selectionStart, field.selectionEnd ?? field.selectionStart);
    return String(document.getSelection() ?? "");
  };
  // Bubbling to window runs after page handlers, before cut removes the selection.
  const copied = (event) =>
    send(event.defaultPrevented ? event.clipboardData?.getData("text/plain") : selection());
  addEventListener("copy", copied);
  addEventListener("cut", copied);
  const proto = globalThis.Clipboard?.prototype;
  if (!proto) return;
  const writeText = proto.writeText;
  proto.writeText = function (text) {
    const result = writeText.call(this, text);
    result.then(
      () => send(String(text)),
      () => {},
    );
    return result;
  };
  const write = proto.write;
  proto.write = function (items) {
    const result = write.call(this, items);
    result.then(
      async () => {
        const item = [...items].find((candidate) => candidate.types.includes("text/plain"));
        if (item) send(await (await item.getType("text/plain")).text());
      },
      () => {},
    );
    return result;
  };
})();
