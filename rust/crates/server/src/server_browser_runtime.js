(() => {
  const KEY = "__KEY__";
  if (Object.prototype.hasOwnProperty.call(globalThis, KEY)) return;
  // Stands in for Playwright's injected script: the selector engines an agent's locators use, the checks an action waits on, and the
  // accessibility snapshot with refs. Errors carry a prefix the server reads: `T3:<kind>:<message>`.
  const fail = (kind, message) => {
    throw new Error("T3:" + kind + ":" + message);
  };
  const refs = new Map();
  let counter = 0;
  const normalize = (text) =>
    String(text ?? "")
      .replace(/\s+/g, " ")
      .trim();
  const quoted = (body) => {
    const m = /^(["'`])((?:\\.|(?!\1)[^\\])*)\1(\s*[si])?$/s.exec(body);
    return m
      ? { text: m[2].replace(/\\(.)/g, "$1"), exact: true, flag: (m[3] || "").trim() }
      : null;
  };
  const matcher = (body) => {
    const re = /^\/((?:\\.|[^/\\])+)\/([a-z]*)$/.exec(body);
    if (re) {
      const pattern = new RegExp(re[1], re[2]);
      return (text) => pattern.test(text);
    }
    const q = quoted(body);
    if (q) {
      const want = normalize(q.text);
      return q.flag === "s" || q.flag === ""
        ? (text) => normalize(text) === want
        : (text) => normalize(text).toLowerCase() === want.toLowerCase();
    }
    const want = normalize(body).toLowerCase();
    return (text) => normalize(text).toLowerCase().includes(want);
  };
  const splitChain = (selector) => {
    const parts = [];
    let current = "";
    let quote = null;
    for (let i = 0; i < selector.length; i++) {
      const c = selector[i];
      if (quote) {
        current += c;
        if (c === "\\") current += selector[++i] ?? "";
        else if (c === quote) quote = null;
        continue;
      }
      if (c === '"' || c === "'" || c === "`") {
        quote = c;
        current += c;
        continue;
      }
      if (c === ">" && selector[i + 1] === ">") {
        parts.push(current);
        current = "";
        i++;
        continue;
      }
      current += c;
    }
    parts.push(current);
    return parts.map((part) => part.trim());
  };
  const ENGINES = new Set([
    "css",
    "xpath",
    "text",
    "role",
    "id",
    "data-testid",
    "data-test-id",
    "data-test",
    "nth",
    "aria-ref",
  ]);
  const parsePart = (part, whole) => {
    if (part.startsWith("//") || part.startsWith("..")) return { name: "xpath", body: part };
    const m = /^([\w\-+:*]+)\s*=\s*([\s\S]*)$/.exec(part);
    if (!m) return { name: "css", body: part };
    const name = m[1].replace(/^\*/, "");
    if (!ENGINES.has(name))
      fail("invalid", 'Unknown engine "' + name + '" while parsing selector ' + whole);
    return { name, body: m[2] };
  };
  const shadowRoots = (root) => {
    const found = [];
    const walk = (node) => {
      for (const el of node.querySelectorAll("*")) {
        if (el.shadowRoot) {
          found.push(el.shadowRoot);
          walk(el.shadowRoot);
        }
      }
    };
    walk(root);
    return found;
  };
  const deep = (root, fn) => {
    const out = [...fn(root)];
    for (const shadow of root.nodeType === 1 || root.nodeType === 9 || root.nodeType === 11
      ? shadowRoots(root)
      : [])
      out.push(...fn(shadow));
    return out;
  };
  const isVisible = (el) => {
    const style = getComputedStyle(el);
    if (style.visibility === "hidden" || style.visibility === "collapse") return false;
    const rect = el.getBoundingClientRect();
    return rect.width > 0 && rect.height > 0;
  };
  const textOf = (el) =>
    el.tagName === "SCRIPT" || el.tagName === "STYLE" ? "" : el.textContent || "";
  const IMPLICIT = {
    A: (el) => (el.hasAttribute("href") ? "link" : null),
    AREA: (el) => (el.hasAttribute("href") ? "link" : null),
    ARTICLE: () => "article",
    ASIDE: () => "complementary",
    BUTTON: () => "button",
    DETAILS: () => "group",
    DIALOG: () => "dialog",
    FIELDSET: () => "group",
    FOOTER: () => "contentinfo",
    FORM: () => "form",
    H1: () => "heading",
    H2: () => "heading",
    H3: () => "heading",
    H4: () => "heading",
    H5: () => "heading",
    H6: () => "heading",
    HEADER: () => "banner",
    HR: () => "separator",
    IMG: (el) => (el.getAttribute("alt") === "" ? "presentation" : "img"),
    INPUT: (el) => {
      const type = (el.getAttribute("type") || "text").toLowerCase();
      if (["button", "submit", "reset", "image"].includes(type)) return "button";
      if (type === "checkbox") return "checkbox";
      if (type === "radio") return "radio";
      if (type === "range") return "slider";
      if (type === "number") return "spinbutton";
      if (type === "search") return el.hasAttribute("list") ? "combobox" : "searchbox";
      if (type === "hidden") return null;
      return el.hasAttribute("list") ? "combobox" : "textbox";
    },
    LI: () => "listitem",
    MAIN: () => "main",
    NAV: () => "navigation",
    OL: () => "list",
    OPTION: () => "option",
    P: () => "paragraph",
    PROGRESS: () => "progressbar",
    SECTION: (el) =>
      el.hasAttribute("aria-label") || el.hasAttribute("aria-labelledby") ? "region" : null,
    SELECT: (el) => (el.multiple || el.size > 1 ? "listbox" : "combobox"),
    SUMMARY: () => "button",
    TABLE: () => "table",
    TBODY: () => "rowgroup",
    TD: () => "cell",
    TEXTAREA: () => "textbox",
    TFOOT: () => "rowgroup",
    TH: () => "columnheader",
    THEAD: () => "rowgroup",
    TR: () => "row",
    UL: () => "list",
  };
  const roleOf = (el) => {
    const explicit = (el.getAttribute("role") || "").trim().split(/\s+/)[0];
    if (explicit) return explicit;
    return IMPLICIT[el.tagName]?.(el) ?? null;
  };
  const FROM_CONTENT = new Set([
    "button",
    "link",
    "heading",
    "cell",
    "columnheader",
    "rowheader",
    "listitem",
    "option",
    "tab",
    "menuitem",
    "checkbox",
    "radio",
    "article",
    "row",
    "tooltip",
    "treeitem",
    "switch",
    "gridcell",
  ]);
  const labelText = (el) => {
    const ids = (el.getAttribute("aria-labelledby") || "").split(/\s+/).filter(Boolean);
    if (ids.length) {
      const root = el.getRootNode();
      return ids
        .map((id) =>
          normalize(
            textOf(root.getElementById?.(id) || document.getElementById(id) || { textContent: "" }),
          ),
        )
        .join(" ");
    }
    return "";
  };
  const contentName = (el, depth) => {
    let out = "";
    for (const child of el.childNodes) {
      if (child.nodeType === 3) out += child.textContent;
      else if (child.nodeType === 1 && depth < 8) {
        if (!isVisible(child) && child.tagName !== "OPTION") continue;
        out +=
          " " +
          (child.getAttribute("aria-label") ||
            (child.tagName === "IMG"
              ? child.getAttribute("alt") || ""
              : contentName(child, depth + 1))) +
          " ";
      }
    }
    return out;
  };
  const nameOf = (el) => {
    const labelled = labelText(el);
    if (labelled) return labelled;
    const aria = el.getAttribute("aria-label");
    if (aria) return normalize(aria);
    const tag = el.tagName;
    if (tag === "IMG" || (tag === "INPUT" && el.type === "image"))
      return normalize(el.getAttribute("alt") || el.getAttribute("title") || "");
    if (tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT") {
      if (tag === "INPUT" && ["button", "submit", "reset"].includes(el.type))
        return normalize(
          el.value || (el.type === "submit" ? "Submit" : el.type === "reset" ? "Reset" : ""),
        );
      const labels = el.labels
        ? [...el.labels].map((label) => normalize(contentName(label, 0))).filter(Boolean)
        : [];
      if (labels.length) return labels.join(" ");
      return normalize(el.getAttribute("placeholder") || el.getAttribute("title") || "");
    }
    if (FROM_CONTENT.has(roleOf(el))) {
      const content = normalize(contentName(el, 0));
      if (content) return content;
    }
    return normalize(el.getAttribute("title") || "");
  };
  const ROLE_ATTRS = {
    checked: (el) =>
      el.type === "checkbox" || el.type === "radio"
        ? el.checked
        : el.getAttribute("aria-checked") === "true",
    pressed: (el) => el.getAttribute("aria-pressed") === "true",
    selected: (el) =>
      el.tagName === "OPTION" ? el.selected : el.getAttribute("aria-selected") === "true",
    expanded: (el) =>
      el.tagName === "DETAILS" ? el.open : el.getAttribute("aria-expanded") === "true",
    disabled: (el) => el.matches(":disabled") || el.getAttribute("aria-disabled") === "true",
    level: (el) =>
      /^H[1-6]$/.test(el.tagName)
        ? Number(el.tagName[1])
        : Number(el.getAttribute("aria-level") || 0),
  };
  const roleEngine = (root, body) => {
    const m = /^\s*([a-zA-Z-]+)\s*((?:\[[^\]]*(?:"[^"]*"|'[^']*')?[^\]]*\]\s*)*)$/.exec(body);
    if (!m) fail("invalid", "Unexpected token in role selector " + body);
    const role = m[1];
    const options = {};
    const re =
      /\[\s*([\w-]+)\s*(?:=\s*(?:"((?:[^"\\]|\\.)*)"(?:\s*([si]))?|'((?:[^'\\]|\\.)*)'(?:\s*([si]))?|([^\]\s]+)))?\s*\]/g;
    for (let a = re.exec(m[2]); a; a = re.exec(m[2])) {
      const value = a[2] ?? a[4] ?? a[6];
      options[a[1]] = {
        value: value === undefined ? true : value.replace(/\\(.)/g, "$1"),
        flag: a[3] ?? a[5] ?? "",
      };
    }
    const includeHidden =
      options["include-hidden"]?.value === true || options["include-hidden"]?.value === "true";
    return deep(root, (scope) => [...scope.querySelectorAll("*")]).filter((el) => {
      if (roleOf(el) !== role) return false;
      if (!includeHidden && !isVisible(el)) return false;
      for (const [key, { value, flag }] of Object.entries(options)) {
        if (key === "include-hidden") continue;
        if (key === "name") {
          const name = nameOf(el);
          const want = String(value);
          if (
            flag === "s"
              ? name !== want
              : !normalize(name).toLowerCase().includes(normalize(want).toLowerCase())
          )
            return false;
        } else if (ROLE_ATTRS[key]) {
          const actual = ROLE_ATTRS[key](el);
          if (String(actual) !== String(value === true ? true : value)) return false;
        } else fail("invalid", "Unknown attribute " + key + " in role selector");
      }
      return true;
    });
  };
  const textEngine = (root, body) => {
    const test = matcher(body);
    const all = deep(root, (scope) => [...scope.querySelectorAll("*")]);
    const matching = new Set(all.filter((el) => test(textOf(el))));
    return all.filter(
      (el) => matching.has(el) && ![...el.children].some((child) => matching.has(child)),
    );
  };
  // `button:has-text("Send")`, `:text(...)`, `:text-is(...)` and `:visible` are Playwright's, at the end of a compound selector.
  const PSEUDO =
    /:(has-text|text-is|text|visible)(?:\(\s*("(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*')\s*\))?\s*$/;
  const cssEngine = (root, css) => {
    const filters = [];
    let base = css;
    for (let m = PSEUDO.exec(base); m; m = PSEUDO.exec(base)) {
      base = base.slice(0, m.index);
      const arg = m[2] ? m[2].slice(1, -1).replace(/\\(.)/g, "$1") : "";
      if (m[1] === "visible") filters.push(isVisible);
      else if (m[1] === "has-text")
        filters.push((el) =>
          normalize(textOf(el)).toLowerCase().includes(normalize(arg).toLowerCase()),
        );
      else if (m[1] === "text-is") filters.push((el) => normalize(textOf(el)) === normalize(arg));
      else {
        const test = matcher(m[2] ?? '""');
        filters.push(
          (el) => test(textOf(el)) && ![...el.children].some((child) => test(textOf(child))),
        );
      }
    }
    let found;
    try {
      found = deep(root, (scope) => [...scope.querySelectorAll(base.trim() || "*")]);
    } catch (error) {
      fail(
        "invalid",
        "Unexpected token in selector " +
          css +
          " while parsing selector: " +
          (error?.message ?? error),
      );
    }
    return found.filter((el) => filters.every((filter) => filter(el)));
  };
  const engine = {
    css: cssEngine,
    xpath: (root, body) => {
      try {
        const doc = root.ownerDocument || root;
        const snapshot = doc.evaluate(
          body,
          root,
          null,
          XPathResult.ORDERED_NODE_SNAPSHOT_TYPE,
          null,
        );
        const out = [];
        for (let i = 0; i < snapshot.snapshotLength; i++)
          if (snapshot.snapshotItem(i).nodeType === 1) out.push(snapshot.snapshotItem(i));
        return out;
      } catch (error) {
        fail("invalid", "Invalid xpath " + body + ": " + (error?.message ?? error));
      }
    },
    text: textEngine,
    role: roleEngine,
    id: (root, body) =>
      deep(root, (scope) => [...scope.querySelectorAll("[id]")]).filter(
        (el) => el.id === (quoted(body)?.text ?? body.trim()),
      ),
    "aria-ref": (root, body) => {
      const el = refs.get(body.trim())?.deref();
      if (!el || !el.isConnected)
        fail("stale", "This element ref is stale. Take a fresh snapshot.");
      return [el];
    },
  };
  for (const attribute of ["data-testid", "data-test-id", "data-test"]) {
    engine[attribute] = (root, body) =>
      deep(root, (scope) => [...scope.querySelectorAll("[" + attribute + "]")]).filter(
        (el) => el.getAttribute(attribute) === (quoted(body)?.text ?? body.trim()),
      );
  }
  const query = (selector) => {
    let scope = [document];
    for (const part of splitChain(selector)) {
      const { name, body } = parsePart(part, selector);
      if (name === "nth") {
        const index = Number(body.trim());
        if (!Number.isInteger(index)) fail("invalid", "Unexpected token in nth selector " + body);
        scope = [scope.at(index)].filter(Boolean);
        continue;
      }
      const next = [];
      for (const root of scope)
        for (const el of engine[name](root, body)) if (!next.includes(el)) next.push(el);
      scope = next;
    }
    return scope.filter((el) => el.nodeType === 1);
  };
  const locate = (selector) => {
    const found = query(selector);
    if (found.length > 1)
      fail("invalid", "strict mode violation: locator resolved to " + found.length + " elements");
    return found[0] ?? null;
  };
  const deepElementFromPoint = (x, y) => {
    let el = document.elementFromPoint(x, y);
    while (el && el.shadowRoot) {
      const inner = el.shadowRoot.elementFromPoint(x, y);
      if (!inner || inner === el) break;
      el = inner;
    }
    return el;
  };
  const frames = () =>
    new Promise((resolve) => {
      let settled = false;
      const done = () => {
        if (!settled) {
          settled = true;
          resolve();
        }
      };
      requestAnimationFrame(() => requestAnimationFrame(done));
      setTimeout(done, 100);
    });
  const describe = (el) => (el ? el.tagName.toLowerCase() + (el.id ? "#" + el.id : "") : "nothing");
  const T3 = {
    count: (selector) => query(selector).length,
    // Scrolls to the element and says whether an action can land on it now: `missing`, `retry` with why, or `ok` with the point.
    probe: async (selector, options) => {
      const el = locate(selector);
      if (!el) return { status: "missing" };
      if (options.scroll)
        el.scrollIntoView({ block: "center", inline: "center", behavior: "instant" });
      if (!isVisible(el)) return { status: "retry", reason: "element is not visible" };
      if (options.enabled && (el.matches(":disabled") || el.closest("[aria-disabled=true]")))
        return { status: "retry", reason: "element is not enabled" };
      const before = el.getBoundingClientRect();
      await frames();
      const rect = el.getBoundingClientRect();
      if (!el.isConnected) return { status: "retry", reason: "element was detached from the DOM" };
      if (
        rect.x !== before.x ||
        rect.y !== before.y ||
        rect.width !== before.width ||
        rect.height !== before.height
      )
        return { status: "retry", reason: "element is not stable" };
      const x = rect.x + rect.width / 2;
      const y = rect.y + rect.height / 2;
      if (options.hit) {
        const hit = deepElementFromPoint(x, y);
        const labelled = el.closest("label");
        if (
          !hit ||
          !(
            hit === el ||
            el.contains(hit) ||
            (hit.getRootNode() instanceof ShadowRoot &&
              hit.getRootNode().host &&
              el.contains(hit.getRootNode().host)) ||
            (labelled && labelled.contains(hit))
          )
        )
          return { status: "retry", reason: describe(hit) + " intercepts pointer events" };
      }
      return { status: "ok", x, y };
    },
    point: (selector) => {
      const el = locate(selector);
      if (!el) return null;
      const rect = el.getBoundingClientRect();
      return { x: rect.x + rect.width / 2, y: rect.y + rect.height / 2 };
    },
    // Focuses what `type` will write to; false when it is not an enabled text control.
    focusEditable: (selector) => {
      const el =
        selector === null
          ? ([...document.querySelectorAll("*:focus")].pop() ?? null)
          : locate(selector);
      if (!el) return null;
      const nonText = [
        "button",
        "checkbox",
        "color",
        "file",
        "hidden",
        "image",
        "radio",
        "range",
        "reset",
        "submit",
      ];
      const textControl =
        el.tagName === "TEXTAREA" ||
        (el.tagName === "INPUT" && !nonText.includes(el.type ?? "text"));
      if (!(textControl || el.isContentEditable) || el.disabled || el.readOnly) return false;
      el.focus?.();
      const root = el.getRootNode();
      const active = root.activeElement;
      return active != null && (active === el || el.contains(active));
    },
    // Fills a text control directly, as `locator.fill` does.
    fill: (selector, text) => {
      const el = locate(selector);
      if (!el) return null;
      const editable =
        el.isContentEditable ||
        el.tagName === "TEXTAREA" ||
        (el.tagName === "INPUT" &&
          ![
            "button",
            "checkbox",
            "file",
            "hidden",
            "image",
            "radio",
            "range",
            "reset",
            "submit",
          ].includes(el.type));
      if (!editable || el.disabled || el.readOnly) return false;
      el.focus();
      if (el.isContentEditable) {
        const selection = getSelection();
        selection.selectAllChildren(el);
        document.execCommand("insertText", false, text);
      } else {
        el.select?.();
        const proto =
          el.tagName === "TEXTAREA" ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
        Object.getOwnPropertyDescriptor(proto, "value").set.call(el, text);
        el.dispatchEvent(new Event("input", { bubbles: true }));
        el.dispatchEvent(new Event("change", { bubbles: true }));
      }
      return true;
    },
    selectInfo: (selector) => {
      const el = locate(selector);
      if (!el) return undefined;
      return el.tagName === "SELECT"
        ? [...el.options].map((option) => ({ value: option.value, label: option.label.trim() }))
        : null;
    },
    selectOptions: (selector, values) => {
      const el = locate(selector);
      if (!el || el.tagName !== "SELECT") return null;
      const wanted = new Set(values);
      for (const option of el.options) option.selected = wanted.has(option.value);
      el.dispatchEvent(new Event("input", { bubbles: true, composed: true }));
      el.dispatchEvent(new Event("change", { bubbles: true }));
      return [...el.selectedOptions].map((option) => option.value);
    },
    scrollBy: (selector, x, y) => {
      if (selector === null) {
        scrollBy(x, y);
        return true;
      }
      const el = locate(selector);
      if (!el) return null;
      el.scrollBy(x, y);
      return true;
    },
    // Whether the selector names an `<input type=file>`: `missing`, `notInput` or `ok`.
    fileInputState: (selector) => {
      const el = locate(selector);
      if (!el) return "missing";
      return el.tagName === "INPUT" && el.type === "file" ? "ok" : "notInput";
    },
    fileInput: (selector) => locate(selector),
    attribute: (element, name) => element.getAttribute(name),
    snapshot: () => {
      refs.clear();
      const lines = [];
      const SKIP = new Set(["SCRIPT", "STYLE", "NOSCRIPT", "TEMPLATE", "HEAD"]);
      const LEAF = new Set([
        "heading",
        "button",
        "link",
        "option",
        "tab",
        "menuitem",
        "treeitem",
        "checkbox",
        "radio",
        "switch",
        "tooltip",
        "textbox",
        "searchbox",
        "slider",
        "spinbutton",
        "img",
      ]);
      const INTERACTIVE = new Set([
        "link",
        "button",
        "textbox",
        "searchbox",
        "checkbox",
        "radio",
        "combobox",
        "listbox",
        "option",
        "slider",
        "spinbutton",
        "switch",
        "tab",
        "menuitem",
      ]);
      const flags = (el, role) => {
        const out = [];
        if (role === "heading") out.push("[level=" + ROLE_ATTRS.level(el) + "]");
        if (ROLE_ATTRS.checked(el) && ["checkbox", "radio", "switch"].includes(role))
          out.push("[checked]");
        if (ROLE_ATTRS.disabled(el) && INTERACTIVE.has(role)) out.push("[disabled]");
        if (el.getAttribute("aria-expanded") === "true" || (el.tagName === "DETAILS" && el.open))
          out.push("[expanded]");
        if (el.getAttribute("aria-pressed") === "true") out.push("[pressed]");
        if (el.getAttribute("aria-selected") === "true" || (el.tagName === "OPTION" && el.selected))
          out.push("[selected]");
        return out.join(" ");
      };
      const visit = (el, depth) => {
        if (
          SKIP.has(el.tagName) ||
          !(isVisible(el) || el.children.length > 0 || el.tagName === "OPTION")
        )
          return;
        const style = getComputedStyle(el);
        if (style.display === "none" || el.getAttribute("aria-hidden") === "true") return;
        const role = roleOf(el);
        const pad = "  ".repeat(depth);
        let childDepth = depth;
        const interesting = role && role !== "presentation" && role !== "none";
        if (interesting || el.tagName === "IFRAME" || el.tagName === "FRAME") {
          const ref = "e" + ++counter;
          refs.set(ref, new WeakRef(el));
          const name = el.tagName === "IFRAME" || el.tagName === "FRAME" ? "" : nameOf(el);
          let line =
            pad +
            "- " +
            (role || "iframe") +
            (name ? ' "' + name.replace(/"/g, '\\"').slice(0, 200) + '"' : "");
          const extra = flags(el, role);
          if (extra) line += " " + extra;
          line += " [ref=" + ref + "]";
          if (style.cursor === "pointer" && INTERACTIVE.has(role)) line += " [cursor=pointer]";
          if (el.tagName === "A" && el.href)
            lines.push(line + ":", pad + "  - /url: " + el.getAttribute("href"));
          else lines.push(line);
          childDepth = depth + 1;
          // These roles are named by their content, so the content is not listed again below them.
          if (LEAF.has(role) || el.tagName === "IFRAME" || el.tagName === "FRAME") {
            if ((el.tagName === "INPUT" || el.tagName === "TEXTAREA") && el.value)
              lines.push(pad + "  - text: " + JSON.stringify(el.value.slice(0, 200)));
            return;
          }
        }
        if (el.shadowRoot) for (const child of el.shadowRoot.children) visit(child, childDepth);
        for (const child of el.childNodes) {
          if (child.nodeType === 3) {
            const text = normalize(child.textContent);
            if (text)
              lines.push("  ".repeat(childDepth) + "- text: " + JSON.stringify(text.slice(0, 500)));
          } else if (child.nodeType === 1) visit(child, childDepth);
        }
      };
      if (document.body) visit(document.body, 0);
      return {
        url: location.href,
        title: document.title,
        loading: document.readyState !== "complete",
        visibleText: (document.body?.innerText || "").slice(0, 20000),
        tree: lines.join("\n"),
      };
    },
  };
  Object.defineProperty(globalThis, KEY, {
    value: T3,
    enumerable: false,
    configurable: false,
    writable: false,
  });
})();
