// Menu bar with shaded drop-down menus, and a Win98-style combo box (shaded drop-down list).

/** Menu bar: click a title to open its menu; while one is open, hovering another switches. */
export function initMenus(bar) {
  const titles = [...bar.querySelectorAll("[data-menu]")];
  let open = null;
  const close = () => {
    if (!open) return;
    open.setAttribute("aria-expanded", "false");
    document.getElementById(open.dataset.menu).hidden = true;
    open = null;
  };
  const show = (t) => {
    close();
    open = t;
    t.setAttribute("aria-expanded", "true");
    const menu = document.getElementById(t.dataset.menu);
    if (!menu.classList.contains("menu-right")) menu.style.left = `${t.offsetLeft}px`;
    menu.hidden = false;
    menu.querySelector("a, button")?.focus({ preventScroll: true });
  };
  for (const t of titles) {
    t.addEventListener("click", (e) => {
      e.stopPropagation();
      open === t ? close() : show(t);
    });
    t.addEventListener("mouseenter", () => open && open !== t && show(t));
    const menu = document.getElementById(t.dataset.menu);
    menu.addEventListener("click", (e) => {
      if (e.target.closest("a, button")) close();
    });
    menu.addEventListener("keydown", (e) => {
      const items = [...menu.querySelectorAll("a, button")];
      const i = items.indexOf(document.activeElement);
      if (e.key === "ArrowDown") items[(i + 1) % items.length].focus(), e.preventDefault();
      if (e.key === "ArrowUp") items[(i - 1 + items.length) % items.length].focus(), e.preventDefault();
      if (e.key === "Escape") close(), t.focus();
    });
  }
  document.addEventListener("click", close);
  return { close };
}

/**
 * Combo box. `options`: [{ value, label, detail, group }]. Calls onChange(value).
 * Keyboard: Up/Down/Home/End, Enter to pick, Escape to close, typing jumps to a match.
 */
export function initCombo(root, { options = [], value = null, placeholder = "", onChange = () => {} } = {}) {
  const field = root.querySelector(".combo-field");
  const pop = root.querySelector(".combo-pop");
  const search = root.querySelector(".combo-search");
  const list = root.querySelector(".combo-list");
  let opts = options;
  let current = value;
  let active = -1;
  let query = "";

  const label = () => {
    const o = opts.find((x) => x.value === current);
    field.querySelector(".combo-value").textContent = o ? `${o.label}${o.detail ? " — " + o.detail : ""}` : placeholder;
  };

  // Indices of the options matching the search: ticker, name or address, any part of it.
  const visible = () => {
    const q = query.trim().toLowerCase();
    return opts
      .map((o, i) => i)
      .filter((i) => !q || [opts[i].label, opts[i].detail ?? "", opts[i].value].some((t) => t.toLowerCase().includes(q)));
  };

  const render = () => {
    list.innerHTML = "";
    let group = null;
    const shown = visible();
    for (const i of shown) {
      const o = opts[i];
      if (o.group !== group) {
        group = o.group;
        const g = document.createElement("li");
        g.className = "combo-group";
        g.setAttribute("role", "presentation");
        g.textContent = group;
        list.append(g);
      }
      const li = document.createElement("li");
      li.id = `${list.id}-${i}`;
      li.setAttribute("role", "option");
      li.setAttribute("aria-selected", String(o.value === current));
      li.dataset.index = i;
      li.innerHTML = `<span class="combo-sym"></span><span class="combo-detail"></span>`;
      li.firstChild.textContent = o.label;
      li.lastChild.textContent = o.detail ?? "";
      if (o.disabled) li.setAttribute("aria-disabled", "true");
      list.append(li);
    }
    if (!shown.length) {
      const li = document.createElement("li");
      li.className = "combo-empty";
      li.setAttribute("role", "presentation");
      li.textContent = query ? `Nothing matches "${query}".` : "Nothing to choose yet.";
      list.append(li);
    }
  };

  const highlight = (i) => {
    list.querySelector(".active")?.classList.remove("active");
    active = i;
    const li = list.querySelector(`[data-index="${i}"]`);
    if (!li) return;
    li.classList.add("active");
    (search ?? field).setAttribute("aria-activedescendant", li.id);
    li.scrollIntoView({ block: "nearest" });
  };

  const isOpen = () => !(pop ?? list).hidden;
  const openList = (seed = "") => {
    query = seed;
    if (search) search.value = seed;
    render();
    (pop ?? list).hidden = false;
    field.setAttribute("aria-expanded", "true");
    const shown = visible();
    highlight(seed ? shown[0] ?? -1 : Math.max(0, opts.findIndex((o) => o.value === current)));
    search?.focus();
  };
  const closeList = (refocus = false) => {
    if (!isOpen()) return;
    (pop ?? list).hidden = true;
    field.setAttribute("aria-expanded", "false");
    if (refocus) field.focus();
  };
  const pick = (i) => {
    const o = opts[i];
    if (!o || o.disabled) return;
    current = o.value;
    label();
    closeList(true);
    onChange(current);
  };

  // Up/down move through what the search shows, skipping disabled options.
  const step = (d) => {
    const shown = visible().filter((i) => !opts[i].disabled);
    if (!shown.length) return;
    const at = shown.indexOf(active);
    highlight(shown[at < 0 ? 0 : Math.min(shown.length - 1, Math.max(0, at + d))]);
  };
  const navKeys = (e) => {
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      if (!isOpen()) return openList();
      step(e.key === "ArrowDown" ? 1 : -1);
      return true;
    }
    if (e.key === "Enter") {
      e.preventDefault();
      isOpen() ? pick(active) : openList();
      return true;
    }
    if (e.key === "Escape" && isOpen()) {
      e.preventDefault();
      closeList(true);
      return true;
    }
    return false;
  };

  field.addEventListener("click", (e) => {
    e.stopPropagation();
    isOpen() ? closeList() : openList();
  });
  pop?.addEventListener("click", (e) => e.stopPropagation());
  list.addEventListener("click", (e) => {
    e.stopPropagation();
    const li = e.target.closest("[role=option]");
    if (li) pick(+li.dataset.index);
  });
  list.addEventListener("mousemove", (e) => {
    const li = e.target.closest("[role=option]");
    if (li && +li.dataset.index !== active) highlight(+li.dataset.index);
  });
  field.addEventListener("keydown", (e) => {
    if (navKeys(e)) return;
    if (e.key === " ") {
      e.preventDefault();
      openList();
    } else if (e.key.length === 1 && !e.ctrlKey && !e.metaKey && !e.altKey) {
      // Typing on the closed box starts a search.
      e.preventDefault();
      openList(e.key);
    }
  });
  search?.addEventListener("keydown", navKeys);
  search?.addEventListener("input", () => {
    query = search.value;
    render();
    highlight(visible().find((i) => !opts[i].disabled) ?? -1);
  });
  document.addEventListener("click", () => closeList());

  render();
  label();
  return {
    setOptions(next, val = current) {
      opts = next;
      current = val;
      render();
      label();
    },
    get value() {
      return current;
    },
  };
}
